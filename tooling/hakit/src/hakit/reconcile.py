"""Each stream's records against its generator's own counters (ADR-0049 §4.5, §3.12).

The records plus the calls not placed plus the calls the generator failed
equal its offered-call counter; a difference is `record_gap`, and so are the
records dropped on their way to the disk (`lost_records`) and a record copy
that did not complete (`records_complete` false), reconciled stream or not. A
count of calls the generator failed above 0 is `generator_failure`.

A stream whose calls without a record are known and at most the profile's
`record_shortfall_pct` of its calls keeps these findings marked `tolerated`:
they leave the run valid.
"""

from __future__ import annotations

from collections import Counter

from . import jsonval as jv
from .jsonval import Json


def integrity(kind: str, detail: str, **extra: object) -> Json:
    return {"kind": kind, "detail": detail, **extra}


def tolerated(f: Json) -> bool:
    return f.get("tolerated") is True


def _shortfall(calls: int | None, records: int, not_placed: int, complete: bool, pct: float) -> int | None:
    """The calls without a record when their count is known and within `pct`
    percent of `calls`; else None."""
    if calls is None or not complete:
        return None
    missing = calls - records - not_placed
    return missing if 0 < missing <= calls * pct / 100 else None


def reconcile(
    streams_doc: list[Json],
    records: Counter[str],
    success: Counter[str],
    stalls: dict[str, list[Json]],
    shortfall_pct: float = 0.0,
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
        mine: list[Json] = []
        if lost:
            why = "the generator's record buffer was full or a disk write failed"
            mine.append(integrity("record_gap", f"{lost} records dropped on their way to the disk: {why}", stream=name))
        if not complete:
            why = "its records on disk may be short and their losses uncounted"
            mine.append(integrity("record_gap", f"the generator's record copy did not complete: {why}", stream=name))
        if failed:
            mine.append(integrity("generator_failure", f"the generator failed {failed} calls without a record", stream=name))
        if calls is None and reconciled:
            mine.append(integrity("record_gap", f"the generator's call counter could not be read; {n} records", stream=name))
        elif calls is not None and reconciled and n + not_placed + (failed or 0) != calls:
            what = f"{n} records + {not_placed} not placed"
            if failed is not None:
                what += f" + {failed} failed by the generator"
            mine.append(integrity("record_gap", f"{what} != {calls} calls the generator counted", stream=name))
        short = _shortfall(calls if reconciled else None, n, not_placed, complete, shortfall_pct)
        if short is not None and calls is not None:
            note = f"tolerated: {short} of {calls} calls without a record, within {shortfall_pct:g} %"
            mine = [{**f, "tolerated": True, "detail": f"{jv.string(f, 'detail')} ({note})"} for f in mine]
        found += mine
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
