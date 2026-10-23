"""Each stream's records against its generator's own counters (ADR-0049 §4.5, §3.12).

The records plus the calls not placed plus the calls the generator failed
equal its offered-call counter; a difference is `record_gap`, and so are the
records dropped on their way to the disk (`lost_records`) and a record copy
that did not complete (`records_complete` false), reconciled stream or not. A
count of calls the generator failed above 0 is `generator_failure`.
"""

from __future__ import annotations

from collections import Counter

from . import jsonval as jv
from .jsonval import Json


def integrity(kind: str, detail: str, **extra: object) -> Json:
    return {"kind": kind, "detail": detail, **extra}


def reconcile(
    streams_doc: list[Json], records: Counter[str], success: Counter[str], stalls: dict[str, list[Json]]
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
        if lost:
            why = "the generator's record buffer was full or a disk write failed"
            found.append(integrity("record_gap", f"{lost} records dropped on their way to the disk: {why}", stream=name))
        if not jv.boolean(s, "records_complete", True):
            why = "its records on disk may be short and their losses uncounted"
            found.append(integrity("record_gap", f"the generator's record copy did not complete: {why}", stream=name))
        if failed:
            found.append(integrity("generator_failure", f"the generator failed {failed} calls without a record", stream=name))
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
