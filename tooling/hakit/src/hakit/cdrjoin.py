"""The CDR join: the run's `cdr-observation.v1` lines against its records (ADR-0049 §3.8).

Byte-identical observations (a broker redelivery, a re-post) count once.
Joined on `call_id`, each mismatch of a kind (its impact's `reason`):
`missing`, an answered call with no CDR, charged at its `ended_at_ms` (unless
the export was cut with CDRs still owed), and an unanswered one the
profile's `cdr_required` names; `extra`, a CDR with no record, at its own
`end_ms`; `duplicate`, each distinct CDR after the first of a call, at the
record's `ended_at_ms`; `differs`, a disposition other than the caller saw,
and for a planned teardown (`ending: answered`) a duration that differs from
the caller's by more than 1 s plus the CDR's unit, at the record's
`ended_at_ms` (for a `cut` or `stranded` call, a CDR ending later than the
caller's end plus that tolerance). A `differs` impact carries its excess past
the tolerance (`excess_ms`); a disposition that differs carries none.
"""

from __future__ import annotations

import json
import math
from collections import defaultdict
from collections.abc import Callable

from . import jsonval as jv
from .classify import Impact
from .jsonval import Json

CDR_MISMATCH = "cdr_mismatch"
DURATION_TOLERANCE_MS = 1000
MISSING, EXTRA, DUPLICATE, DIFFERS = "missing", "extra", "duplicate", "differs"
KINDS = (MISSING, EXTRA, DUPLICATE, DIFFERS)
NO_CDR = "answered call without a CDR"
NO_REQUIRED_CDR = "a call that must have a CDR without one"


def impact(
    stream: str, call_id: str, at_ms: float, kind: str, detail: str, excess_ms: float = math.inf, started_ms: float | None = None
) -> Impact:
    """A mismatch charged at `at_ms`, of a call alive from `started_ms` (`at_ms` when unknown)."""
    started = at_ms if started_ms is None else started_ms
    return Impact(CDR_MISMATCH, stream, call_id, at_ms, started, reason=kind, detail=detail, excess_ms=excess_ms)


def _compare(r: Json, cdr: Json) -> tuple[str, float] | None:
    """Why `cdr` does not match record `r`, with its excess past the
    tolerance (infinite for a disposition), or None."""
    answered_at = jv.opt_number(r, "answered_at_ms")
    disposition = jv.string(cdr, "disposition")
    expected = "answered" if answered_at is not None else "not_answered"
    if disposition != expected:
        return f"CDR {disposition}, the caller saw the call {expected.replace('_', ' ')}", math.inf
    if answered_at is None:
        return None
    allowed = DURATION_TOLERANCE_MS + jv.number(cdr, "duration_unit_ms")
    ending = jv.string(r, "ending")
    if ending in ("cut", "stranded"):
        over = jv.number(cdr, "end_ms") - jv.number(r, "ended_at_ms")
        if over <= allowed:
            return None
        return f"CDR ends {over:.0f} ms after the caller's end (allowed {allowed:g} ms)", over - allowed
    if ending != "answered":
        return None
    caller_ms = jv.number(r, "ended_at_ms") - answered_at
    cdr_ms = jv.number(cdr, "duration_ms")
    apart = abs(cdr_ms - caller_ms)
    if apart > allowed:
        return f"CDR duration {cdr_ms:g} ms, the caller's {caller_ms:g} ms (allowed {allowed:g} ms apart)", apart - allowed
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
        started = jv.number(r, "started_at_ms")
        mine = by_call.get(call_id, [])
        if not mine:
            if charge_missing and jv.opt_number(r, "answered_at_ms") is not None:
                out.append(impact(stream, call_id, ended, MISSING, NO_CDR, started_ms=started))
            elif charge_missing and required(r):
                out.append(impact(stream, call_id, ended, MISSING, NO_REQUIRED_CDR, started_ms=started))
            continue
        why = _compare(r, mine[0])
        if why is not None:
            out.append(impact(stream, call_id, ended, DIFFERS, why[0], why[1], started_ms=started))
        out += [
            impact(stream, call_id, ended, DUPLICATE, f"CDR {i + 2} of the call", started_ms=started)
            for i in range(len(mine) - 1)
        ]
    for call_id, mine in sorted(by_call.items()):
        if call_id not in seen:
            out += [
                impact("", call_id, jv.number(c, "end_ms"), EXTRA, "CDR without a record", started_ms=jv.number(c, "start_ms"))
                for c in mine
            ]
    return out
