"""A run directory written by hand, for the verdict to recompute."""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from pathlib import Path

from kit import T0, write_jsonl

from hakit import jsonval as jv
from hakit.content import discover
from hakit.jsonval import Json
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet
from hakit.windows import event_line, phase_line

SCHEMAS = SchemaSet.load()
CONTENT = discover([BASE_CONTENT], SCHEMAS)
# Hand-written records are not paced at the stream's rate: a gap between two of
# them is no generator stall unless a test says so.
UNPACED: Json = {"generator_stall_ms": 3_600_000}


def run_dir(
    tmp_path: Path,
    records: list[Json],
    calls: int | None,
    events: list[Json],
    integrity: Sequence[Mapping[str, object]] = (),
    *,
    profile: Mapping[str, object] = UNPACED,
    budgets: Sequence[Json] = (),
    stream: Mapping[str, object] | None = None,
    cdrs: list[Json] | None = None,
    classes: Mapping[str, Json] | None = None,
    cps: float = 20,
) -> Path:
    d = tmp_path / "run"
    (d / "budgets").mkdir(parents=True)
    (d / "catalogs").mkdir()
    meta = {"run": "r", "started_at_ms": T0, "load_started_at_ms": T0, "ended_at_ms": T0 + 600_000}
    (d / "run-meta.json").write_text(json.dumps(meta))
    base = CONTENT.profiles["kill-worker-smoke"].raw
    streams = [{**s, "cps": cps} for s in jv.objs(base, "streams")]
    (d / "profile.json").write_text(json.dumps({**base, "streams": streams, **profile}))
    for b in [CONTENT.budgets["worker-kill"].raw, *budgets]:
        (d / "budgets" / f"{b['name']}.json").write_text(json.dumps(b))
    (d / "catalogs" / "x-api-call.json").write_text(json.dumps(CONTENT.catalogs["x-api-call"].raw))
    s = {"stream": "basic", "shape": "basic", "start": "run", "cps": cps, "calls": calls, "not_placed": 0, **(stream or {})}
    (d / "streams.json").write_text(json.dumps([s]))
    write_jsonl(d / "records.jsonl", records)
    write_jsonl(d / "events.jsonl", events)
    write_jsonl(d / "integrity.jsonl", [dict(i) for i in integrity])
    if cdrs is not None:
        write_jsonl(d / "cdrs.jsonl", cdrs)
    if classes is not None:
        (d / "classes").mkdir()
        for name, doc in classes.items():
            (d / "classes" / f"{name}.json").write_text(json.dumps(doc))
    return d


def kill(seq: int, at: int, failed: bool = False, budget: str = "worker-kill", recovered_after: int = 5_000) -> list[Json]:
    """The phases of one kill_worker event injected at `at`."""
    ev = f"{seq:03d}-kill_worker"
    lines = [event_line(ev, "schedule", ["kill_worker"], budget, 40, 220), phase_line(ev, "kill_worker", "inject", at)]
    if failed:
        return [*lines, phase_line(ev, "kill_worker", "failed", at + 100), phase_line(ev, "kill_worker", "heal", at + 100)]
    return [
        *lines,
        phase_line(ev, "kill_worker", "injected", at + 100),
        phase_line(ev, "kill_worker", "heal", at + 100),
        phase_line(ev, "kill_worker", "healed", at + 110),
        phase_line(ev, "kill_worker", "recovered", at + recovered_after),
    ]
