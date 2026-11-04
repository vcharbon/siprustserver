"""The calls established when a window's fault began (budget `established`).

A fault of a call-setup dependency must leave the calls already up whole: a
call answered before the window's first `inject` and not ended at it is
established. Each one must end with no impact its budget does not allow (an
in-dialog request failed, a cut, a stranded teardown, an unexpected ending
after answer) and no CDR mismatch from the fault's start to the next
window's; a call refused a consult under its budget's allowance stays
whole. The window must hold at least the budget's
`min_calls` of them, so a window no call spanned never passes the check,
once the run answered any call before it (a run that placed none, a
dry run, has nothing to judge).
"""

from __future__ import annotations

from collections.abc import Iterable

from . import jsonval as jv
from .classify import Impact
from .jsonval import Json
from .judge import ALL_CALL_IDS, MAX_CALL_IDS
from .windows import Window

ESTABLISHED = "established"


def established_row(
    w: Window, min_calls: int, records: Iterable[Json], hurts: Iterable[tuple[str, float]], until_ms: float
) -> Json:
    """The window's `established` row: the established calls, those an
    unallowed impact hurt from the window's first inject to `until_ms` (the
    next window's, or the end), the count, allowed none, and the finding."""
    inject = w.first("inject")
    hurt = {c for c, at in hurts if inject is not None and inject <= at < until_ms}
    calls: list[str] = []
    answered_before = False
    for r in records:
        answered = jv.opt_number(r, "answered_at_ms")
        if inject is None or answered is None or answered >= inject:
            continue
        answered_before = True
        if jv.number(r, "ended_at_ms") < inject:
            continue
        calls.append(jv.string(r, "call_id"))
    hurt_ids = [c for c in calls if c in hurt]
    too_few = answered_before and len(calls) < min_calls
    return {
        "class": ESTABLISHED,
        "count": len(hurt_ids),
        "allowed": 0,
        "calls": len(calls),
        "min_calls": min_calls,
        "finding": bool(hurt_ids) or too_few,
        "call_ids": hurt_ids[:MAX_CALL_IDS],
        ALL_CALL_IDS: hurt_ids,
    }


def hurts(impacts: Iterable[Impact], counted: Iterable[Impact]) -> list[tuple[str, float]]:
    """Each impact no budget allowance takes, by call and instant: every
    in-dialog impact and every counted one (a CDR mismatch, a product's
    class)."""
    return [(i.call_id, i.at_ms) for i in impacts if not i.budgetable] + [(i.call_id, i.at_ms) for i in counted]


def until_next(windows: list[Window], w: Window) -> float:
    """The first inject of the window after `w`, the end of `w`'s span of
    blame; infinite for the last."""
    inject = w.first("inject")
    later = [i for o in windows if (i := o.first("inject")) is not None and inject is not None and i > inject]
    return min(later, default=float("inf"))
