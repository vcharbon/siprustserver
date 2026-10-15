"""Each window's recovery, its close, and its `recovery` impact rows (ADR-0049 §3.5, §3.6).

Windows are taken in opening order: a window's limit is its incident's maximum
or the next window's opening, whichever comes first. Each stream with a
baseline is searched for its return from `healed` up to the limit; the window
then closes `settle_s` after the latest return (never before the probe's
close), or at the limit when a stream never came back. A `recovery` row per
stream is a finding when the stream never came back, or came back later than
the budget's `recovery.max_s`.
"""

from __future__ import annotations

from collections.abc import Mapping

from .jsonval import Json
from .model import Budget
from .recovery import StreamRecovery, StreamSamples, recover
from .windows import Window

RECOVERY = "recovery"


def _row(r: StreamRecovery, healed_ms: float, limit_ms: float, max_s: float | None) -> Json:
    observed_s = round((limit_ms - healed_ms) / 1000, 3)
    got = r.recovery_s(healed_ms)
    allowed = max_s if max_s is not None else observed_s
    finding = got is None or (max_s is not None and got > max_s)
    return {"class": RECOVERY, "stream": r.stream, "count": observed_s if got is None else got, "allowed": allowed, "finding": finding}


def recover_windows(
    windows: list[Window], samples: Mapping[str, StreamSamples], budgets: Mapping[str, Budget]
) -> dict[str, tuple[list[StreamRecovery], list[Json]]]:
    """By event: the recovery of each stream with a baseline and its rows. Closes
    each window on its recovery as a side effect."""
    out: dict[str, tuple[list[StreamRecovery], list[Json]]] = {}
    ordered = sorted(windows, key=lambda w: w.opened_at_ms)
    for i, w in enumerate(ordered):
        inject = w.first("inject")
        healed = w.last("healed")
        budget = budgets.get(w.budget)
        if w.status != "ok" or inject is None or healed is None or budget is None:
            continue
        limit = w.max_close_ms
        if i + 1 < len(ordered):
            limit = min(limit, ordered[i + 1].opened_at_ms)
        found = [
            r
            for stream, s in sorted(samples.items())
            if (r := recover(stream, s, inject, healed, limit, budget.recovery)) is not None
        ]
        w.close_on_recovery([r.back_at_ms for r in found], limit)
        out[w.event] = (found, [_row(r, healed, limit, budget.recovery.max_s) for r in found])
    return out
