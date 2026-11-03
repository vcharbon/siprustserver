"""The calls established when a window's fault began (budget `established`).

A fault of a call-setup dependency must leave the calls already up whole: a
call answered before the window's first `inject` and not ended at it is
established. Each one must end with no impact its budget does not allow (an
in-dialog request failed, a cut, a stranded teardown, an unexpected ending
after answer) and no CDR mismatch; a call refused a consult under its
budget's allowance stays whole. The window must hold at least the budget's
`min_calls` of them, so a window no call spanned never passes the check.
"""

from __future__ import annotations

from collections.abc import Iterable

from . import jsonval as jv
from .classify import Impact
from .jsonval import Json
from .judge import ALL_CALL_IDS, MAX_CALL_IDS
from .windows import Window

ESTABLISHED = "established"


def established_row(w: Window, min_calls: int, records: Iterable[Json], hurt: set[str]) -> Json:
    """The window's `established` row: the established calls, those an
    unallowed impact hurt (the count, allowed none), and the finding."""
    inject = w.first("inject")
    calls: list[str] = []
    for r in records:
        answered = jv.opt_number(r, "answered_at_ms")
        if inject is None or answered is None or answered >= inject:
            continue
        if jv.number(r, "ended_at_ms") < inject:
            continue
        calls.append(jv.string(r, "call_id"))
    hurt_ids = [c for c in calls if c in hurt]
    return {
        "class": ESTABLISHED,
        "count": len(hurt_ids),
        "allowed": 0,
        "calls": len(calls),
        "min_calls": min_calls,
        "finding": bool(hurt_ids) or len(calls) < min_calls,
        "call_ids": hurt_ids[:MAX_CALL_IDS],
        ALL_CALL_IDS: hurt_ids,
    }


def hurt_calls(impacts: Iterable[Impact], counted: Iterable[Impact]) -> set[str]:
    """The calls an impact no budget allowance takes hurt: every in-dialog
    impact and every counted one (a CDR mismatch, a product's class)."""
    return {i.call_id for i in impacts if not i.budgetable} | {i.call_id for i in counted}
