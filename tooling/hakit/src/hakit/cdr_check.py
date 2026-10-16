"""The CDR side of a verdict (ADR-0049 §3.8): the export's completeness, then the join.

The collector leaves `cdrs.jsonl` and `cdrs-collect.json` (whether every CDR
the workers wrote was consumed before the cut, the counts, the payloads the
consumer could not read). What makes the CDR measurement doubtful is a
`class_failed` integrity finding: an export cut with CDRs still owed (whose
missing CDRs are then not charged), unreadable payloads or observations, and no
observation at all while answered calls exist. The CDRs of a firing stopped
with `abandon` are left out: its killed calls wrote no record.
"""

from __future__ import annotations

import json
from pathlib import Path

from . import jsonval as jv
from .cdrjoin import join
from .classify import Impact
from .jsonval import Json, as_obj
from .profile import Profile
from .reconcile import integrity
from .schemas import SchemaSet
from .jsonl import read_jsonl


def _failed(detail: str) -> Json:
    return integrity("class_failed", f"cdr_mismatch: {detail}")


def abandoned_firings(run_dir: Path, run: str) -> list[str]:
    """The Call-ID marks (`<run>-<event>`) of the load incidents' firings stopped
    with `abandon`: their calls wrote no record, so their CDRs join none."""
    return [
        f"{run}-{jv.string(line, 'event')}"
        for line in read_jsonl(run_dir / "events.jsonl")
        if line.get("kind") == "facts"
        and line.get("executable") == "heal"
        and jv.obj(line, "facts").get("stop") == "abandon"
    ]


def cdr_impacts_of(run_dir: Path, profile: Profile, records: list[Json], schemas: SchemaSet) -> tuple[list[Impact], list[Json]]:
    if not profile.cdr_join:
        return [], []
    p, cp = run_dir / "cdrs.jsonl", run_dir / "cdrs-collect.json"
    if not p.is_file() or not cp.is_file():
        return [], [_failed("the run directory holds no CDR export")]
    collect = as_obj(json.loads(cp.read_text()))
    found: list[Json] = []
    complete = jv.boolean(collect, "complete")
    if not complete:
        counts = [jv.opt_number(collect, k) for k in ("written", "consumed")]
        written, consumed = ("unread" if v is None else f"{v:g}" for v in counts)
        found.append(_failed(f"the export was cut with CDRs still owed: {written} written, {consumed} consumed"))
    unread = jv.opt_number(collect, "unread") or 0
    if unread:
        found.append(_failed(f"the consumer could not read {unread:g} CDR payloads"))
    good: list[Json] = []
    bad = 0
    run = jv.string(as_obj(json.loads((run_dir / "run-meta.json").read_text())), "run")
    abandoned = abandoned_firings(run_dir, run)
    for line in read_jsonl(p):
        if schemas.errors("cdr-observation.v1", line):
            bad += 1
        elif not any(mark in str(line.get("call_id")) for mark in abandoned):
            good.append(line)
    if bad:
        found.append(_failed(f"{bad} observations break cdr-observation.v1"))
    answered = sum(1 for r in records if jv.opt_number(r, "answered_at_ms") is not None)
    if not good and answered:
        found.append(_failed(f"no CDR observation for {answered} answered calls; is the consumer shipping?"))
        return [], found
    required = profile.cdr_required
    return join(records, good, charge_missing=complete, required=lambda r: any(m.matches(r) for m in required)), found
