"""Window attribution and budget evaluation (ADR-0049 §3.2, §3.3, §3.5, §3.8, §3.10).

A budgetable impact belongs to the window that holds its instant and is
charged to the first budget entry that matches it; a key no entry names allows
0. A counted impact (`cdr_mismatch` by kind, a product's class) is charged to
its window's allowance for the class and kind; a missing CDR whose call ended
just before a window opened, within the sink's in-flight span, belongs to that
window. One outside every window is `unattributed`.
An in-dialog impact takes no budget: it carries every window whose inject fell
inside the call's lifetime before the impact or that holds the impact's
instant, and with none it is `unattributed`. A slow setup measured across a generator stall is no impact:
it is returned apart, with its stall.
"""

from __future__ import annotations

from collections import defaultdict
from collections.abc import Callable, Sequence
from dataclasses import dataclass

from .cdrjoin import CDR_MISMATCH, MISSING
from .classify import SETUP_FAILED, SETUP_SLOW, UNEXPECTED_ENDING, Impact, SetupLatency
from .jsonval import Json
from .model import NO_BUDGET, Allowance, Budget
from .stalls import Stall
from .windows import Window

MAX_CALL_IDS = 50
ALL_CALL_IDS = "all_call_ids"

type StallOf = Callable[[SetupLatency], Stall | None]


@dataclass(frozen=True)
class Judged:
    per_window: dict[str, list[Json]]
    in_dialog: list[Json]
    unattributed: list[Json]
    stalled: list[tuple[SetupLatency, Stall]]


def _row(cls: str, stream: str, items: Sequence[Impact | SetupLatency], allowed: float, cps: float) -> Json:
    count = len(items)
    row: Json = {"class": cls}
    if stream:
        row["stream"] = stream
    row |= {
        "count": count,
        "allowed": allowed,
        "finding": count > allowed,
        "call_ids": [i.call_id for i in items if i.call_id][:MAX_CALL_IDS],
        # Every Call-ID, kept apart from the verdict (`impact-calls.jsonl`).
        ALL_CALL_IDS: [i.call_id for i in items if i.call_id],
    }
    if cps > 0:
        row["seconds"] = round(count / cps, 3)
    return row


def _keyed(
    cls: str, impacts: list[Impact], entries: list[Allowance], cps: dict[str, float], outage_s: float = 0.0
) -> list[Json]:
    charged: dict[tuple[int, str], list[Impact]] = defaultdict(list)
    unmatched: dict[tuple[str, int, str, bool], list[Impact]] = defaultdict(list)
    for imp in impacts:
        code, reason = imp.code or 0, imp.reason or ""
        idx = next((i for i, e in enumerate(entries) if e.matches(imp.stream, code, reason, imp.retry_after)), None)
        if idx is None:
            unmatched[(imp.stream, code, reason, imp.retry_after)].append(imp)
        else:
            charged[(idx, imp.stream)].append(imp)
    rows: list[Json] = []
    for (idx, stream), items in sorted(charged.items()):
        e = entries[idx]
        rate = cps.get(stream, 0.0)
        row = {**_row(cls, stream, items, e.allowed(rate, outage_s), rate), "code": e.code, "reason": e.reason}
        if e.retry_after is not None:
            row["retry_after"] = e.retry_after
        rows.append(row)
    for (stream, code, reason, retry_after), items in sorted(unmatched.items()):
        rate = cps.get(stream, 0.0)
        row = {**_row(cls, stream, items, 0, rate), "code": code, "reason": reason}
        if retry_after:
            row["retry_after"] = True
        rows.append(row)
    return rows


def outage_s(w: Window) -> float:
    """The window's outage in seconds: its first `inject` to its last `recovered`
    (to its last `healed` without one); 0 when it has neither."""
    start = w.first("inject")
    end = w.last("recovered") or w.last("healed")
    if start is None or end is None:
        return 0.0
    return max(0.0, (end - start) / 1000.0)


def _counted_allowance(budget: Budget, cls: str, kind: str, items: Sequence[Impact], run_cps: float, outage: float) -> float:
    """A counted class's allowance in a window; for a `cdr_mismatch` kind, its
    allowance plus every impact within the kind's `excess_ms`."""
    if cls != CDR_MISMATCH:
        return budget.classes.get(cls, 0.0)
    a = budget.cdr_mismatch.get(kind)
    if a is None:
        return 0.0
    within = 0 if a.excess_ms is None else sum(1 for i in items if i.excess_ms <= a.excess_ms)
    return a.allowed(run_cps, outage) + within


def _holding(windows: list[Window], at_ms: float) -> Window | None:
    return next((w for w in windows if w.holds(at_ms)), None)


def _budget_window(windows: list[Window], imp: Impact) -> Window | None:
    """The window a budgetable impact belongs to: the one holding its instant;
    for a setup that timed out outside every window, the first window open
    while it was pending (its INVITE went out into the fault no hop answered,
    and the generator gave up after the window closed)."""
    w = _holding(windows, imp.at_ms)
    if w is not None or imp.cls != SETUP_FAILED or imp.reason != "timeout":
        return w
    return next((w for w in windows if w.opened_at_ms <= imp.at_ms and imp.started_at_ms <= w.closed_at_ms), None)


def _counted_window(windows: list[Window], imp: Impact, in_flight_ms: float) -> Window | None:
    """The window a counted impact belongs to: the one holding its instant; for
    a missing CDR outside every window, the first opening within `in_flight_ms`
    after the call's end, since its CDR was still in the sink then."""
    w = _holding(windows, imp.at_ms)
    if w is not None or imp.cls != CDR_MISMATCH or imp.reason != MISSING:
        return w
    return next((w for w in windows if imp.at_ms < w.opened_at_ms <= imp.at_ms + in_flight_ms), None)


def budget_of(w: Window, budgets: dict[str, Budget]) -> Budget:
    if not w.budget:
        return NO_BUDGET
    budget = budgets.get(w.budget)
    if budget is None:
        raise ValueError(f"window {w.event}: budget {w.budget!r} is not in the run directory")
    return budget


def judge(
    windows: list[Window],
    impacts: list[Impact],
    latencies: list[SetupLatency],
    budgets: dict[str, Budget],
    setup_slow_ms: float,
    cps: dict[str, float],
    counted: Sequence[Impact] = (),
    stall_of: StallOf = lambda _lat: None,
    run_cps: float = 0.0,
    cdr_in_flight_ms: float = 0.0,
) -> Judged:
    """`run_cps` is the summed offered rate of the streams that run for the
    whole load, the rate a counted allowance in seconds reads;
    `cdr_in_flight_ms` how long a CDR stays in the system under test's sink
    after its call ends (the profile's `cdr_in_flight_s`)."""
    in_window: dict[str, list[Impact]] = defaultdict(list)
    counted_in_window: dict[str, list[Impact]] = defaultdict(list)
    slow_in_window: dict[str, list[SetupLatency]] = defaultdict(list)
    outside: dict[tuple[str, str, int, str, bool], list[Impact | SetupLatency]] = defaultdict(list)
    in_dialog: list[Json] = []
    stalled: list[tuple[SetupLatency, Stall]] = []

    for imp in impacts:
        if imp.budgetable:
            w = _budget_window(windows, imp)
            if w is None:
                outside[(imp.cls, imp.stream, imp.code or 0, imp.reason or "", imp.retry_after)].append(imp)
            else:
                in_window[w.event].append(imp)
            continue
        candidates = [
            w.event
            for w in windows
            if ((inj := w.first("inject")) is not None and imp.started_at_ms <= inj <= imp.at_ms) or w.holds(imp.at_ms)
        ]
        if not candidates:
            outside[(imp.cls, imp.stream, imp.code or 0, imp.reason or "", False)].append(imp)
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

    for imp in counted:
        w = _counted_window(windows, imp, cdr_in_flight_ms)
        if w is None:
            outside[(imp.cls, "", 0, imp.reason or "", False)].append(imp)
        else:
            counted_in_window[w.event].append(imp)

    for lat in latencies:
        w = _holding(windows, lat.at_ms)
        budget = budget_of(w, budgets) if w is not None else None
        threshold = setup_slow_ms
        if budget is not None and budget.setup_slow.threshold_ms is not None:
            threshold = budget.setup_slow.threshold_ms
        if lat.ms <= threshold:
            continue
        stall = stall_of(lat)
        if stall is not None:
            stalled.append((lat, stall))
        elif w is None:
            outside[(SETUP_SLOW, lat.stream, 0, "", False)].append(lat)
        else:
            slow_in_window[w.event].append(lat)

    per_window: dict[str, list[Json]] = {}
    for w in windows:
        budget = budget_of(w, budgets)
        mine = in_window.get(w.event, [])
        out_s = outage_s(w)
        rows = _keyed(SETUP_FAILED, [i for i in mine if i.cls == SETUP_FAILED], budget.setup_failed, cps, out_s)
        rows += _keyed(
            UNEXPECTED_ENDING, [i for i in mine if i.cls != SETUP_FAILED], budget.unexpected_ending, cps, out_s
        )
        by_stream: dict[str, list[SetupLatency]] = defaultdict(list)
        for lat in slow_in_window.get(w.event, []):
            by_stream[lat.stream].append(lat)
        for stream, lats in sorted(by_stream.items()):
            rate = cps.get(stream, 0.0)
            rows.append(_row(SETUP_SLOW, stream, lats, budget.setup_slow.allowed(rate, out_s), rate))
        by_class: dict[tuple[str, str], list[Impact]] = defaultdict(list)
        for imp in counted_in_window.get(w.event, []):
            by_class[(imp.cls, imp.reason or "")].append(imp)
        for (cls, kind), items in sorted(by_class.items()):
            row = _row(cls, "", items, _counted_allowance(budget, cls, kind, items, run_cps, out_s), 0.0)
            if kind:
                row["reason"] = kind
            rows.append(row)
        per_window[w.event] = rows

    unattributed: list[Json] = []
    for (cls, stream, code, reason, retry_after), items in sorted(outside.items()):
        row = _row(cls, stream, items, 0, cps.get(stream, 0.0))
        row["finding"] = True
        if cls in (SETUP_FAILED, UNEXPECTED_ENDING):
            row["code"] = code
        if reason:
            row["reason"] = reason
        if retry_after:
            row["retry_after"] = True
        unattributed.append(row)
    return Judged(per_window=per_window, in_dialog=in_dialog, unattributed=unattributed, stalled=stalled)
