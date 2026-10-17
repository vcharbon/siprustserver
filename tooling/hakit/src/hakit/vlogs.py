"""VictoriaLogs: the call-outcome records and CDR observations of a run, counted and exported.

A shipped line carries the record verbatim in `_msg` and its stream fields
(`run`, `source`, `stream`) beside it (ADR-0049 §4.4); the export parses
`_msg` back, so a record leaves VictoriaLogs exactly as the generator wrote it.
The CDR consumer ships its observations the same way under `source`
`cdr-consumer` (§3.8).
"""

from __future__ import annotations

import json
import urllib.parse

from .executor import Executor
from .jsonval import Json, as_obj


CDR_FILTER = '_stream:{source="cdr-consumer"}'


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
        time range: byte-identical lines (a redelivery) count once."""
        return self._count(f"{CDR_FILTER} {json.dumps(run)}", "cdrs", start_ms, end_ms, stat="count_uniq(_msg)")

    def export_cdrs(self, run: str, start_ms: int, end_ms: int) -> tuple[list[Json], list[str]]:
        """The CDR observations whose Call-ID carries the run id, and the lines that
        did not parse back into one."""
        status, body = self._query(CDR_FILTER, start_ms, end_ms)
        if status != 200:
            return [], [f"VictoriaLogs export failed: HTTP {status}: {body[:200]}"]
        out: list[Json] = []
        bad: list[str] = []
        for obs in self._messages(body, bad):
            if run in str(obs.get("call_id", "")):
                out.append(obs)
        return out, bad

    @staticmethod
    def _messages(body: str, bad: list[str]) -> list[Json]:
        out: list[Json] = []
        for line in body.splitlines():
            if not line.strip():
                continue
            try:
                row = as_obj(json.loads(line))
                out.append(as_obj(json.loads(str(row.get("_msg", "")))))
            except ValueError:
                bad.append(line[:200])
        return out

    def export(self, run: str, start_ms: int, end_ms: int) -> tuple[list[Json], list[str]]:
        """The run's records, and the lines that did not parse back into one."""
        status, body = self._query(_stream_filter(run, None), start_ms, end_ms)
        if status != 200:
            return [], [f"VictoriaLogs export failed: HTTP {status}: {body[:200]}"]
        bad: list[str] = []
        records = self._messages(body, bad)
        return records, bad
