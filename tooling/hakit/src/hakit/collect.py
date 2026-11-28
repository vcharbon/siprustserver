"""After the drain: the run's CDR observations into `cdrs.jsonl` (ADR-0049 §3.8).

The CDR chain lags the calls (a frozen broker delivers after its thaw, the
consumer ships in batches), so the export waits until the run's distinct
shipped observations cover every CDR the workers wrote and have stopped growing
for `QUIET_S`, at most `MAX_WAIT_S`. The observations, not the consumer's
counter, are the measure: a consumer killed between two scrapes takes what it
counted since the last one with it, while what it shipped stays.

The written count is what the workers' counter series counted after the run
started (`counters.increase`): it holds no CDR of an earlier run, and a worker
replaced or restarted during the run counts from its new series. A worker gone
between two scrapes takes what it counted since the last one with it, so once a
written series ended or reset in the run (`counters.lossy`) the count is a lower
bound, and covering it proves nothing. The export therefore also waits for the
broker's queue to read empty in a scrape taken after the observations stopped
growing: the consumer acknowledges a CDR once its observation landed, so an
empty queue holds none still owed. Without that reading (the lab names no queue
series, or the broker was not scraped since) only an exact written count
completes the export. A CDR the broker lost (a broker replaced with its queue)
is owed no more: cdr_check charges it missing.
`cdrs-collect.json` records whether that happened (`complete`), the counts, and
the payloads the consumer could not read; the verdict judges them.
"""

from __future__ import annotations

import json
from pathlib import Path

from .clock import Clock
from .counters import BASELINE_LOOKBACK_MS, increase, lossy
from .jsonval import Json
from .lab import Lab
from .vlogs import VictoriaLogs
from .vm import QueryFailed, RawSeries, VictoriaMetrics

QUIET_S = 10
MAX_WAIT_S = 300
POLL_S = 2
MARGIN_MS = 3_600_000


def _since_start(vm: VictoriaMetrics, selector: str, started_ms: int, now_ms: int) -> list[RawSeries] | None:
    """The counter `selector`'s series over the run, with each one's last sample
    before it; `None` when VictoriaMetrics does not answer or holds no series."""
    try:
        series = vm.export(selector, started_ms - BASELINE_LOOKBACK_MS, now_ms)
    except QueryFailed:
        return None
    return series or None


def _counted(vm: VictoriaMetrics, selector: str, started_ms: int, now_ms: int) -> float | None:
    """What the counter `selector` counted since the run started; `None` when
    VictoriaMetrics does not answer or holds no series of it."""
    series = _since_start(vm, selector, started_ms, now_ms)
    return increase(series, started_ms) if series is not None else None


def _queued(vm: VictoriaMetrics, selector: str, since_ms: int, now_ms: int) -> float | None:
    """The CDRs the broker's queue holds: the newest sample at or after
    `since_ms` of each of the gauge's series that did not end, summed; `None`
    when the lab names no gauge, VictoriaMetrics does not answer, or no live
    series was scraped since."""
    if not selector:
        return None
    try:
        series = vm.export(selector, since_ms, now_ms)
    except QueryFailed:
        return None
    newest = [s.points[-1][1] for s in series if s.ended_ms is None and s.points]
    return sum(newest) if newest else None


def _none_owed(queued: float | None, exact: bool) -> bool:
    """Whether observations that cover the written count hold every CDR: the
    queue reads empty or, with no reading, the written count is exact."""
    return queued == 0 if queued is not None else exact


def collect_cdrs(
    vlogs: VictoriaLogs, vm: VictoriaMetrics, lab: Lab, clock: Clock, run: str, run_dir: Path, started_ms: int
) -> list[Json]:
    """Write `cdrs.jsonl` and `cdrs-collect.json`; the integrity findings of the export itself."""
    deadline = clock.now_ms() + MAX_WAIT_S * 1000
    last: int | None = None
    quiet_since = clock.now_ms()
    written: float | None = None
    exact = False
    queued: float | None = None
    complete = False
    while True:
        series = _since_start(vm, lab.cdr_written_series, started_ms, clock.now_ms())
        written = increase(series, started_ms) if series is not None else None
        exact = series is not None and not lossy(series, started_ms)
        n = vlogs.cdr_count(run, started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
        if n != last:
            last, quiet_since = n, clock.now_ms()
        caught_up = written is not None and last is not None and last >= written
        if caught_up and clock.now_ms() - quiet_since >= QUIET_S * 1000:
            queued = _queued(vm, lab.cdr_queued_series, quiet_since, clock.now_ms())
            if _none_owed(queued, exact):
                complete = True
                break
        if clock.now_ms() >= deadline:
            break
        clock.sleep(POLL_S)
    if not complete:
        queued = _queued(vm, lab.cdr_queued_series, quiet_since, clock.now_ms())
    unread = _counted(vm, lab.cdr_unread_series, started_ms, clock.now_ms())
    consumed = _counted(vm, lab.cdr_consumed_series, started_ms, clock.now_ms())
    bad: list[str] = []
    observations = 0
    with (run_dir / "cdrs.jsonl").open("w") as fh:
        for c in vlogs.export_cdrs(run, started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS, bad):
            fh.write(json.dumps(c) + "\n")
            observations += 1
    doc: Json = {
        "complete": complete,
        "written": written,
        "written_exact": exact,
        "observed": last,
        "queued": queued,
        "consumed": consumed,
        "unread": unread,
        "observations": observations,
    }
    (run_dir / "cdrs-collect.json").write_text(json.dumps(doc, indent=2) + "\n")
    found: list[Json] = []
    if bad:
        found.append({"kind": "class_failed", "detail": f"cdr_mismatch: {len(bad)} observation lines unreadable, first: {bad[0]}"})
    return found
