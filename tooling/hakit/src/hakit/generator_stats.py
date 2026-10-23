"""A generator's own periodic statistics, as the run directory keeps them.

For SIPp, the `-trace_stat` file of the stream (`streams/<name>/stat.csv`,
`;`-separated, one row a second): the instant (`CurrentTime`, its last tab
field in epoch seconds), the calls in flight (`CurrentCall`) and the
watchdog's cumulative counts (`WatchdogMinor(C)` + `WatchdogMajor(C)`).
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
            rows.append(StatRow(at, _int(r.get("CurrentCall")), watchdog))
    rows.sort(key=lambda r: r.at_ms)
    return rows
