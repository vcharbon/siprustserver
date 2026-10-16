"""After the drain: the run's CDR observations into `cdrs.jsonl` (ADR-0049 §3.8).

The CDR chain lags the calls (a frozen broker delivers after its thaw, the
consumer ships in batches), so the export waits until the consumer has consumed
every CDR the workers wrote and the shipped observations have stopped growing
for `QUIET_S`, at most `MAX_WAIT_S`. `cdrs-collect.json` records whether that
happened (`complete`), the counts it compared, and the payloads the consumer
could not read; the verdict judges them.
"""

from __future__ import annotations

import json
from pathlib import Path

from .clock import Clock
from .jsonval import Json
from .lab import Lab
from .vlogs import VictoriaLogs
from .vm import QueryFailed, VictoriaMetrics

QUIET_S = 10
MAX_WAIT_S = 300
POLL_S = 2
MARGIN_MS = 3_600_000


def _read(vm: VictoriaMetrics, template: str, span_s: int) -> float | None:
    try:
        return vm.scalar(template.replace("{span}", str(span_s)))
    except QueryFailed:
        return None


def collect_cdrs(
    vlogs: VictoriaLogs, vm: VictoriaMetrics, lab: Lab, clock: Clock, run: str, run_dir: Path, started_ms: int
) -> list[Json]:
    """Write `cdrs.jsonl` and `cdrs-collect.json`; the integrity findings of the export itself."""
    deadline = clock.now_ms() + MAX_WAIT_S * 1000
    last: int | None = None
    quiet_since = clock.now_ms()
    written: float | None = None
    consumed: float | None = None
    complete = False
    while True:
        span = int((clock.now_ms() - started_ms) / 1000) + 60
        written = _read(vm, lab.cdr_written_query, span)
        consumed = _read(vm, lab.cdr_consumed_query, span)
        n = vlogs.cdr_count(started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
        if n != last:
            last, quiet_since = n, clock.now_ms()
        caught_up = written is not None and consumed is not None and consumed >= written
        if caught_up and last is not None and clock.now_ms() - quiet_since >= QUIET_S * 1000:
            complete = True
            break
        if clock.now_ms() >= deadline:
            break
        clock.sleep(POLL_S)
    span = int((clock.now_ms() - started_ms) / 1000) + 60
    unread = _read(vm, lab.cdr_unread_query, span)
    cdrs, bad = vlogs.export_cdrs(run, started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
    with (run_dir / "cdrs.jsonl").open("w") as fh:
        fh.writelines(json.dumps(c) + "\n" for c in cdrs)
    doc: Json = {"complete": complete, "written": written, "consumed": consumed, "unread": unread, "observations": len(cdrs)}
    (run_dir / "cdrs-collect.json").write_text(json.dumps(doc, indent=2) + "\n")
    found: list[Json] = []
    if bad:
        found.append({"kind": "class_failed", "detail": f"cdr_mismatch: {len(bad)} observation lines unreadable, first: {bad[0]}"})
    return found
