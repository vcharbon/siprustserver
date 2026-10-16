"""The CDR join: the run's `cdr-observation.v1` lines against its records (ADR-0049 §3.8).

Byte-identical observations (a broker redelivery, a re-post) count once.
Joined on `call_id`: an answered call with no CDR is charged at its
`ended_at_ms` (unless the export was cut with CDRs still owed); a CDR with no
record at its own `end_ms`; each distinct CDR after the first of a call, a
disposition that differs, and for a planned teardown (`ending: answered`) a
duration that differs from the caller's by more than 1 s plus the CDR's unit,
at the record's `ended_at_ms`. For a `cut` or `stranded` call the CDR may not
end later than the caller's end plus that tolerance. An unanswered call
without a CDR is no mismatch, unless the profile's `cdr_required` names it.
"""

from __future__ import annotations

import json
from collections import defaultdict
from collections.abc import Callable

from . import jsonval as jv
from .classify import Impact
from .jsonval import Json

CDR_MISMATCH = "cdr_mismatch"
DURATION_TOLERANCE_MS = 1000
NO_CDR = "answered call without a CDR"
NO_REQUIRED_CDR = "a call that must have a CDR without one"


def _impact(stream: str, call_id: str, at_ms: float, detail: str) -> Impact:
    return Impact(CDR_MISMATCH, stream, call_id, at_ms, at_ms, detail=detail)


def _compare(r: Json, cdr: Json) -> str | None:
    """Why `cdr` does not match record `r`, or None."""
    answered_at = jv.opt_number(r, "answered_at_ms")
    disposition = jv.string(cdr, "disposition")
    expected = "answered" if answered_at is not None else "not_answered"
    if disposition != expected:
        return f"CDR {disposition}, the caller saw the call {expected.replace('_', ' ')}"
    if answered_at is None:
        return None
    allowed = DURATION_TOLERANCE_MS + jv.number(cdr, "duration_unit_ms")
    ending = jv.string(r, "ending")
    if ending in ("cut", "stranded"):
        over = jv.number(cdr, "end_ms") - jv.number(r, "ended_at_ms")
        return f"CDR ends {over:.0f} ms after the caller's end (allowed {allowed:g} ms)" if over > allowed else None
    if ending != "answered":
        return None
    caller_ms = jv.number(r, "ended_at_ms") - answered_at
    cdr_ms = jv.number(cdr, "duration_ms")
    if abs(cdr_ms - caller_ms) > allowed:
        return f"CDR duration {cdr_ms:g} ms, the caller's {caller_ms:g} ms (allowed {allowed:g} ms apart)"
    return None


def distinct(cdrs: list[Json]) -> list[Json]:
    """The observations, each byte-identical copy dropped, in order."""
    seen: set[str] = set()
    out: list[Json] = []
    for c in cdrs:
        key = json.dumps(c, sort_keys=True)
        if key not in seen:
            seen.add(key)
            out.append(c)
    return out


def join(
    records: list[Json],
    cdrs: list[Json],
    charge_missing: bool = True,
    required: Callable[[Json], bool] = lambda _r: False,
) -> list[Impact]:
    """The `cdr_mismatch` impacts of a run's schema-valid records and observations.
    `charge_missing` False when the export was cut with CDRs still owed;
    `required` names the calls never answered that must have a CDR too."""
    by_call: dict[str, list[Json]] = defaultdict(list)
    for c in distinct(cdrs):
        by_call[jv.string(c, "call_id")].append(c)
    out: list[Impact] = []
    seen: set[str] = set()
    for r in records:
        call_id = jv.string(r, "call_id")
        if call_id in seen:
            continue
        seen.add(call_id)
        stream = jv.string(r, "stream")
        ended = jv.number(r, "ended_at_ms")
        mine = by_call.get(call_id, [])
        if not mine:
            if charge_missing and jv.opt_number(r, "answered_at_ms") is not None:
                out.append(_impact(stream, call_id, ended, NO_CDR))
            elif charge_missing and required(r):
                out.append(_impact(stream, call_id, ended, NO_REQUIRED_CDR))
            continue
        why = _compare(r, mine[0])
        if why is not None:
            out.append(_impact(stream, call_id, ended, why))
        out += [_impact(stream, call_id, ended, f"CDR {i + 2} of the call") for i in range(len(mine) - 1)]
    for call_id, mine in sorted(by_call.items()):
        if call_id not in seen:
            out += [_impact("", call_id, jv.number(c, "end_ms"), "CDR without a record") for c in mine]
    return out
