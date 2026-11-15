"""A generator's own periodic statistics, as the run directory keeps them.

For SIPp, the `-trace_stat` file of the stream (`streams/<name>/stat.csv`,
`;`-separated, one row a second): the instant (`CurrentTime`, its last tab
field in epoch seconds), the calls in flight (`CurrentCall`), the
watchdog's cumulative counts (`WatchdogMinor(C)` + `WatchdogMajor(C)`) and
the cumulative count of calls SIPp ended as failed (`FailedCall(C)`).
"""

from __future__ import annotations

import csv
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class StatRow:
    at_ms: float
    current_calls: int | None
    watchdog: int | None
    failed: int | None = None


def _int(v: str | None) -> int | None:
    try:
        return int(v) if v not in (None, "") else None
    except ValueError:
        return None


def read_stat_csv(path: Path) -> list[StatRow]:
    """The rows of a SIPp stat file in time order; none when it is absent."""
    if not path.is_file():
        return []
    rows: list[StatRow] = []
    with path.open(newline="") as fh:
        for r in csv.DictReader(fh, delimiter=";"):
            try:
                at = float(str(r.get("CurrentTime") or "").split("\t")[-1]) * 1000
            except ValueError:
                continue
            minor, major = _int(r.get("WatchdogMinor(C)")), _int(r.get("WatchdogMajor(C)"))
            watchdog = None if minor is None and major is None else (minor or 0) + (major or 0)
            rows.append(StatRow(at, _int(r.get("CurrentCall")), watchdog, _int(r.get("FailedCall(C)"))))
    rows.sort(key=lambda r: r.at_ms)
    return rows


@dataclass(frozen=True)
class FailureSpan:
    """`count` calls the generator failed between two statistics rows."""

    from_ms: float
    to_ms: float
    count: int


def failure_spans(rows: list[StatRow]) -> list[FailureSpan]:
    """Where the cumulative failed-call count grows, row to row; a count
    already above 0 in the first row is placed at that row's instant."""
    out: list[FailureSpan] = []
    prev_at, prev_failed = None, 0
    for r in rows:
        if r.failed is None:
            continue
        if r.failed > prev_failed:
            out.append(FailureSpan(prev_at if prev_at is not None else r.at_ms, r.at_ms, r.failed - prev_failed))
        prev_at, prev_failed = r.at_ms, r.failed
    return out
