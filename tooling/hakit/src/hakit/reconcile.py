"""Each stream's records against its generator's own counters (ADR-0049 §4.5, §3.12).

The records plus the calls not placed plus the calls the generator failed
equal its offered-call counter; a difference is `record_gap`, and so are the
records dropped on their way to the disk (`lost_records`) and a record copy
that did not complete (`records_complete` false), reconciled stream or not. A
count of calls the generator failed above 0 is `generator_failure`.

A `generator_failure` is `tolerated`, and leaves the run valid, only when the
counters agree, the generator's statistics place every failed call outside
every window, and the failed calls are at most the profile's
`record_shortfall_pct` of the stream's calls. A failure inside a window may be
a call the system under test broke: it stays a finding naming the window.
"""

from __future__ import annotations

from collections import Counter
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from . import jsonval as jv
from .generator_stats import FailureSpan
from .jsonval import Json


@dataclass(frozen=True)
class Placed:
    """Where a stream's generator failures fell: the count outside every
    window, and the windows that hold the others."""

    outside: int
    windows: tuple[str, ...] = ()


def place(spans: Sequence[FailureSpan], windows: Sequence[tuple[str, float, float]]) -> Placed:
    """The failures of `spans` against the windows (event, opened, closed): a
    span touching a window puts all its calls in that window."""
    outside = 0
    hit: list[str] = []
    for sp in spans:
        inside = [e for e, opened, closed in windows if sp.from_ms <= closed and sp.to_ms >= opened]
        if inside:
            hit += [e for e in inside if e not in hit]
        else:
            outside += sp.count
    return Placed(outside, tuple(hit))


def integrity(kind: str, detail: str, **extra: object) -> Json:
    return {"kind": kind, "detail": detail, **extra}


def tolerated(f: Json) -> bool:
    return f.get("tolerated") is True


def reconcile(
    streams_doc: list[Json],
    records: Counter[str],
    success: Counter[str],
    stalls: dict[str, list[Json]],
    shortfall_pct: float = 0.0,
    placed: Mapping[str, Placed] | None = None,
) -> tuple[list[Json], list[Json]]:
    """The verdict's `streams` and the integrity findings of the reconciliation."""
    out: list[Json] = []
    found: list[Json] = []
    for s in streams_doc:
        name = jv.string(s, "stream")
        calls = jv.opt_int(s, "calls")
        not_placed = jv.opt_int(s, "not_placed") or 0
        failed = jv.opt_int(s, "generator_failures")
        n = records.get(name, 0)
        reconciled = jv.boolean(s, "reconciled", True)
        lost = jv.opt_int(s, "lost_records") or 0
        complete = jv.boolean(s, "records_complete", True)
        if lost:
            why = "the generator's record buffer was full or a disk write failed"
            found.append(integrity("record_gap", f"{lost} records dropped on their way to the disk: {why}", stream=name))
        if not complete:
            why = "its records on disk may be short and their losses uncounted"
            found.append(integrity("record_gap", f"the generator's record copy did not complete: {why}", stream=name))
        if failed:
            where = (placed or {}).get(name, Placed(0))
            found.append(_failure(name, failed, where, calls, shortfall_pct, reconciled and complete and not lost))
        if calls is None and reconciled:
            found.append(integrity("record_gap", f"the generator's call counter could not be read; {n} records", stream=name))
        elif calls is not None and reconciled and n + not_placed + (failed or 0) != calls:
            what = f"{n} records + {not_placed} not placed"
            if failed is not None:
                what += f" + {failed} failed by the generator"
            found.append(integrity("record_gap", f"{what} != {calls} calls the generator counted", stream=name))
        row: Json = {
            "stream": name,
            "shape": jv.string(s, "shape"),
            "start": jv.string(s, "start"),
            "cps": jv.number(s, "cps"),
            "calls": calls,
            "not_placed": not_placed,
            "records": n,
            "success": success.get(name, 0),
            "stalls": stalls.get(name, []),
        }
        if failed is not None:
            row["generator_failures"] = failed
        out.append(row)
    return out, found


def _failure(name: str, failed: int, where: Placed, calls: int | None, pct: float, sound: bool) -> Json:
    """The `generator_failure` of a stream, `tolerated` when its calls are all
    placed outside every window and within `pct` percent of `calls`. `sound`:
    the stream is reconciled and its record copy whole."""
    detail = f"the generator failed {failed} calls without a record"
    if where.windows:
        detail += f"; {failed - where.outside} in window {', '.join(where.windows)}"
    elif where.outside < failed:
        detail += f"; {failed - where.outside} not placed in time"
    if sound and calls is not None and where.outside == failed and failed <= calls * pct / 100:
        note = f"tolerated: {failed} of {calls} calls, all outside every window, within {pct:g} %"
        return integrity("generator_failure", f"{detail} ({note})", stream=name, tolerated=True)
    return integrity("generator_failure", detail, stream=name)
