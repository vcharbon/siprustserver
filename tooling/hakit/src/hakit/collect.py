"""After the drain: the run's CDR observations into `cdrs.jsonl` (ADR-0049 §3.8).

The CDR chain lags the calls (the broker, the consumer's batch), so the runner
waits until the shipped observations stop growing for `QUIET_S`, at most
`MAX_WAIT_S`, then keeps the ones whose Call-ID carries the run id.
"""

from __future__ import annotations

import json
from pathlib import Path

from .clock import Clock
from .jsonval import Json
from .vlogs import VictoriaLogs

QUIET_S = 10
MAX_WAIT_S = 60
POLL_S = 2
MARGIN_MS = 3_600_000


def collect_cdrs(vlogs: VictoriaLogs, clock: Clock, run: str, run_dir: Path, started_ms: int) -> list[Json]:
    """Write `cdrs.jsonl`; the integrity findings of the export."""
    deadline = clock.now_ms() + MAX_WAIT_S * 1000
    last: int | None = None
    quiet_since = clock.now_ms()
    while clock.now_ms() < deadline:
        n = vlogs.cdr_count(started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
        if n != last:
            last, quiet_since = n, clock.now_ms()
        elif clock.now_ms() - quiet_since >= QUIET_S * 1000:
            break
        clock.sleep(POLL_S)
    found: list[Json] = []
    if last is None:
        found.append({"kind": "class_failed", "detail": "cdr_mismatch: VictoriaLogs did not count the CDR observations"})
    cdrs, bad = vlogs.export_cdrs(run, started_ms - MARGIN_MS, clock.now_ms() + MARGIN_MS)
    with (run_dir / "cdrs.jsonl").open("w") as fh:
        fh.writelines(json.dumps(c) + "\n" for c in cdrs)
    if bad:
        found.append({"kind": "class_failed", "detail": f"cdr_mismatch: {len(bad)} observation lines unreadable, first: {bad[0]}"})
    return found
