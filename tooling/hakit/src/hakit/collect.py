"""After the drain: the run's CDR observations into `cdrs.jsonl` (ADR-0049 §3.8).

The CDR chain lags the calls (a frozen broker delivers after its thaw, the
consumer ships in batches), so the export waits until the run's distinct
shipped observations cover every CDR the workers wrote and have stopped growing
for `QUIET_S`, at most `MAX_WAIT_S`. The observations, not the consumer's
counter, are the measure: a consumer killed between two scrapes takes what it
counted since the last one with it, while what it shipped stays. The written
count is what the workers' counter series counted after the run started
(`counters.increase`): it holds no CDR of an earlier run, and a worker
replaced or restarted during the run counts from its new series. A worker
killed between two scrapes takes what it counted since the last one with it,
so the count never exceeds the CDRs written.
`cdrs-collect.json` records whether that happened (`complete`), the counts, and
the payloads the consumer could not read; the verdict judges them.
"""

from __future__ import annotations

import json
from pathlib import Path

from .clock import Clock
from .counters import BASELINE_LOOKBACK_MS, increase
from .jsonval import Json
from .lab import Lab
from .vlogs import VictoriaLogs
from .vm import QueryFailed, VictoriaMetrics

QUIET_S = 10
MAX_WAIT_S = 300
POLL_S = 2
MARGIN_MS = 3_600_000


def _counted(vm: VictoriaMetrics, selector: str, started_ms: int, now_ms: int) -> float | None:
    """What the counter `selector` counted since the run started; `None` when
    VictoriaMetrics does not answer or holds no series of it."""
    try:
        series = vm.export(selector, started_ms - BASELINE_LOOKBACK_MS, now_ms)
    except QueryFailed:
        return None
    return increase(series, started_ms) if series else None


def collect_cdrs(
    vlogs: VictoriaLogs, vm: VictoriaMetrics, lab: Lab, clock: Clock, run: str, run_dir: Path, started_ms: int
) -> list[Json]:
    """Write `cdrs.jsonl` and `cdrs-collect.json`; the integrity findings of the export itself."""
    deadline = clock.now_ms() + MAX_WAIT_S * 1000
    last: int | None = None
    quiet_since = clock.now_ms()
    written: float | None = None
    complete = False
    while True:
        written = _counted(vm, lab.cdr_written_series, started_ms, clock.now_ms())
        n = vlogs.cdr_count(run, started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
        if n != last:
            last, quiet_since = n, clock.now_ms()
        caught_up = written is not None and last is not None and last >= written
        if caught_up and clock.now_ms() - quiet_since >= QUIET_S * 1000:
            complete = True
            break
        if clock.now_ms() >= deadline:
            break
        clock.sleep(POLL_S)
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
        "observed": last,
        "consumed": consumed,
        "unread": unread,
        "observations": observations,
    }
    (run_dir / "cdrs-collect.json").write_text(json.dumps(doc, indent=2) + "\n")
    found: list[Json] = []
    if bad:
        found.append({"kind": "class_failed", "detail": f"cdr_mismatch: {len(bad)} observation lines unreadable, first: {bad[0]}"})
    return found
