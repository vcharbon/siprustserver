"""Each window's recovery, its close, and its `recovery` impact rows (ADR-0049 §3.5, §3.6).

Windows are taken in opening order: a window's limit is its incident's maximum
or the next window's opening, whichever comes first, and its baseline leaves
out the calls started inside the windows before it. Each stream with a
baseline is searched for its return from `healed` up to the limit; the window
then closes `settle_s` after the latest return (never before the probe's
close), or at the limit when a stream never came back. A `recovery` row per
stream is a finding when the stream never came back, or came back later than
the budget's `recovery.max_s`. A stream whose baseline is too small has no row;
the window's facts (`hakit` / `recovery`) say why.
"""

from __future__ import annotations

from collections.abc import Mapping

from .jsonval import Json, as_obj
from .model import Budget
from .recovery import NoBaseline, StreamRecovery, StreamSamples, recover
from .windows import Window

RECOVERY = "recovery"


def _row(r: StreamRecovery, healed_ms: float, limit_ms: float, max_s: float | None) -> Json:
    observed_s = round((limit_ms - healed_ms) / 1000, 3)
    got = r.recovery_s(healed_ms)
    allowed = max_s if max_s is not None else observed_s
    finding = got is None or (max_s is not None and got > max_s)
    return {"class": RECOVERY, "stream": r.stream, "count": observed_s if got is None else got, "allowed": allowed, "finding": finding}


def _note(w: Window, missing: list[NoBaseline]) -> None:
    if not missing:
        return
    hakit = as_obj(w.facts.setdefault("hakit", {}))
    hakit["recovery"] = {m.stream: f"no recovery: {m.reason}" for m in missing}


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
        earlier = [(float(e.opened_at_ms), float(e.closed_at_ms)) for e in ordered[:i]]
        found: list[StreamRecovery] = []
        missing: list[NoBaseline] = []
        for stream, s in sorted(samples.items()):
            r = recover(stream, s, inject, healed, limit, budget.recovery, earlier)
            if isinstance(r, StreamRecovery):
                found.append(r)
            else:
                missing.append(r)
        _note(w, missing)
        w.close_on_recovery([r.back_at_ms for r in found], limit)
        out[w.event] = (found, [_row(r, healed, limit, budget.recovery.max_s) for r in found])
    return out
