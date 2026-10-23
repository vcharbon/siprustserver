"""VictoriaLogs: the call-outcome records and CDR observations of a run, counted and exported.

A shipped line carries the record verbatim in `_msg` and its stream fields
(`run`, `source`, `stream`) beside it (ADR-0049 §4.4); the export parses
`_msg` back, so a record leaves VictoriaLogs exactly as the generator wrote it.
The CDR consumer ships its observations the same way under `source`
`cdr-consumer` (§3.8). An export reads its range one `SLICE_MS` query at a
time and yields as it parses: neither VictoriaLogs nor the runner ever holds a
long run's records in one response.
"""

from __future__ import annotations

import json
import urllib.parse
from collections.abc import Iterator

from .executor import Executor
from .jsonval import Json, as_obj


CDR_FILTER = '_stream:{source="cdr-consumer"}'
SLICE_MS = 600_000


def _stream_filter(run: str, stream: str | None) -> str:
    parts = [f'run="{run}"']
    if stream is not None:
        parts.append(f'stream="{stream}"')
    return "_stream:{" + ",".join(parts) + "}"


class VictoriaLogs:
    def __init__(self, executor: Executor, base_url: str) -> None:
        self.executor = executor
        self.base_url = base_url

    def _query(self, query: str, start_ms: int, end_ms: int) -> tuple[int, str]:
        params = {"query": query, "start": str(start_ms * 1_000_000), "end": str(end_ms * 1_000_000)}
        url = f"{self.base_url}/select/logsql/query?{urllib.parse.urlencode(params)}"
        res = self.executor.http("GET", url, timeout_s=120)
        return res.status, res.body

    def _count(self, selector: str, field: str, start_ms: int, end_ms: int, stat: str = "count()") -> int | None:
        status, body = self._query(f"{selector} | stats {stat} {field}", start_ms, end_ms)
        if status != 200:
            return None
        for line in body.splitlines():
            if line.strip():
                try:
                    return int(str(as_obj(json.loads(line)).get(field, "0")))
                except ValueError:
                    return None
        return 0

    def count(self, run: str, stream: str, start_ms: int, end_ms: int) -> int | None:
        return self._count(_stream_filter(run, stream), "n", start_ms, end_ms)

    def live_count(self, run: str, stream: str, start_ms: int, end_ms: int) -> int | None:
        """The records of a stream so far, read during the load for their freshness."""
        return self._count(_stream_filter(run, stream), "live", start_ms, end_ms)

    def cdr_count(self, run: str, start_ms: int, end_ms: int) -> int | None:
        """The distinct CDR observations whose Call-ID carries the run id, in a
        time range: byte-identical lines (a redelivery) count once. The lines are
        counted by their 64-bit hash, which keeps the query's memory at 8 bytes per
        observation where the lines themselves exceed VictoriaLogs' query budget."""
        return self._count(f"{CDR_FILTER} {json.dumps(run)}", "cdrs", start_ms, end_ms, stat="count_uniq_hash(_msg)")

    def _slices(self, query: str, start_ms: int, end_ms: int, bad: list[str]) -> Iterator[Json]:
        """The `_msg` of every line of the range, one query per slice; a slice that
        fails is one `bad` entry and the export goes on."""
        for lo in range(start_ms, end_ms, SLICE_MS):
            hi = min(lo + SLICE_MS, end_ms)
            status, body = self._query(f"{query} | fields _msg", lo, hi)
            if status != 200:
                bad.append(f"VictoriaLogs export of [{lo}, {hi}) failed: HTTP {status}: {body[:200]}")
                continue
            yield from self._messages(body, bad)

    def export_cdrs(self, run: str, start_ms: int, end_ms: int, bad: list[str]) -> Iterator[Json]:
        """The CDR observations whose Call-ID carries the run id; a line that does
        not parse back into one goes to `bad`."""
        for obs in self._slices(f"{CDR_FILTER} {json.dumps(run)}", start_ms, end_ms, bad):
            if run in str(obs.get("call_id", "")):
                yield obs

    @staticmethod
    def _messages(body: str, bad: list[str]) -> Iterator[Json]:
        for line in body.splitlines():
            if not line.strip():
                continue
            try:
                row = as_obj(json.loads(line))
                yield as_obj(json.loads(str(row.get("_msg", ""))))
            except ValueError:
                bad.append(line[:200])

    def export(self, run: str, start_ms: int, end_ms: int, bad: list[str]) -> Iterator[Json]:
        """The run's records; a line that does not parse back into one goes to `bad`."""
        return self._slices(_stream_filter(run, None), start_ms, end_ms, bad)
