"""Window attribution and budget evaluation (ADR-0049 §3.2, §3.3, §3.5).

A budgetable impact belongs to the window that holds its instant and is
charged to the first budget entry that matches it; a key no entry names allows
0. One outside every window is `unattributed`. An in-dialog impact takes no
budget: it carries every window whose inject fell inside the call's lifetime
before the impact, and with none it is `unattributed`.
"""

from __future__ import annotations

from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass

from .classify import SETUP_FAILED, SETUP_SLOW, UNEXPECTED_ENDING, Impact, SetupLatency
from .jsonval import Json
from .model import Allowance, Budget
from .windows import Window

MAX_CALL_IDS = 50


@dataclass(frozen=True)
class Judged:
    per_window: dict[str, list[Json]]
    in_dialog: list[Json]
    unattributed: list[Json]


def _row(cls: str, stream: str, items: Sequence[Impact | SetupLatency], allowed: float, cps: float) -> Json:
    count = len(items)
    row: Json = {
        "class": cls,
        "stream": stream,
        "count": count,
        "allowed": allowed,
        "finding": count > allowed,
        "call_ids": [i.call_id for i in items[:MAX_CALL_IDS]],
    }
    if cps > 0:
        row["seconds"] = round(count / cps, 3)
    return row


def _keyed(cls: str, impacts: list[Impact], entries: list[Allowance], cps: dict[str, float]) -> list[Json]:
    charged: dict[tuple[int, str], list[Impact]] = defaultdict(list)
    unmatched: dict[tuple[str, int, str], list[Impact]] = defaultdict(list)
    for imp in impacts:
        code, reason = imp.code or 0, imp.reason or ""
        idx = next((i for i, e in enumerate(entries) if e.matches(imp.stream, code, reason)), None)
        if idx is None:
            unmatched[(imp.stream, code, reason)].append(imp)
        else:
            charged[(idx, imp.stream)].append(imp)
    rows: list[Json] = []
    for (idx, stream), items in sorted(charged.items()):
        e = entries[idx]
        rate = cps.get(stream, 0.0)
        rows.append({**_row(cls, stream, items, e.allowed(rate), rate), "code": e.code, "reason": e.reason})
    for (stream, code, reason), items in sorted(unmatched.items()):
        rate = cps.get(stream, 0.0)
        rows.append({**_row(cls, stream, items, 0, rate), "code": code, "reason": reason})
    return rows


def _holding(windows: list[Window], at_ms: float) -> Window | None:
    return next((w for w in windows if w.holds(at_ms)), None)


def judge(
    windows: list[Window],
    impacts: list[Impact],
    latencies: list[SetupLatency],
    budgets: dict[str, Budget],
    setup_slow_ms: float,
    cps: dict[str, float],
) -> Judged:
    in_window: dict[str, list[Impact]] = defaultdict(list)
    slow_in_window: dict[str, list[SetupLatency]] = defaultdict(list)
    outside: dict[tuple[str, str, int, str], list[Impact | SetupLatency]] = defaultdict(list)
    in_dialog: list[Json] = []

    for imp in impacts:
        if imp.budgetable:
            w = _holding(windows, imp.at_ms)
            if w is None:
                outside[(imp.cls, imp.stream, imp.code or 0, imp.reason or "")].append(imp)
            else:
                in_window[w.event].append(imp)
            continue
        candidates = [
            w.event for w in windows if (inj := w.first("inject")) is not None and imp.started_at_ms <= inj <= imp.at_ms
        ]
        if not candidates:
            outside[(imp.cls, imp.stream, imp.code or 0, imp.reason or "")].append(imp)
            continue
        entry: Json = {
            "class": imp.cls,
            "stream": imp.stream,
            "call_id": imp.call_id,
            "at_ms": imp.at_ms,
            "candidate_windows": candidates,
        }
        if imp.detail:
            entry["detail"] = imp.detail
        in_dialog.append(entry)

    for lat in latencies:
        w = _holding(windows, lat.at_ms)
        budget = budgets.get(w.budget) if w is not None else None
        threshold = setup_slow_ms
        if budget is not None and budget.setup_slow.threshold_ms is not None:
            threshold = budget.setup_slow.threshold_ms
        if lat.ms <= threshold:
            continue
        if w is None:
            outside[(SETUP_SLOW, lat.stream, 0, "")].append(lat)
        else:
            slow_in_window[w.event].append(lat)

    per_window: dict[str, list[Json]] = {}
    for w in windows:
        budget = budgets.get(w.budget)
        if budget is None:
            raise ValueError(f"window {w.event}: budget {w.budget!r} is not in the run directory")
        mine = in_window.get(w.event, [])
        rows = _keyed(SETUP_FAILED, [i for i in mine if i.cls == SETUP_FAILED], budget.setup_failed, cps)
        rows += _keyed(UNEXPECTED_ENDING, [i for i in mine if i.cls != SETUP_FAILED], budget.unexpected_ending, cps)
        by_stream: dict[str, list[SetupLatency]] = defaultdict(list)
        for lat in slow_in_window.get(w.event, []):
            by_stream[lat.stream].append(lat)
        for stream, lats in sorted(by_stream.items()):
            rate = cps.get(stream, 0.0)
            rows.append(_row(SETUP_SLOW, stream, lats, budget.setup_slow.allowed(rate), rate))
        per_window[w.event] = rows

    unattributed: list[Json] = []
    for (cls, stream, code, reason), items in sorted(outside.items()):
        row = _row(cls, stream, items, 0, cps.get(stream, 0.0))
        row["finding"] = True
        if cls in (SETUP_FAILED, UNEXPECTED_ENDING):
            row["code"] = code
        if reason:
            row["reason"] = reason
        unattributed.append(row)
    return Judged(per_window=per_window, in_dialog=in_dialog, unattributed=unattributed)
