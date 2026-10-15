"""VictoriaLogs: the call-outcome records of a run, counted and exported.

A shipped line carries the record verbatim in `_msg` and its stream fields
(`run`, `source`, `stream`) beside it (ADR-0049 §4.4); the export parses
`_msg` back, so a record leaves VictoriaLogs exactly as the generator wrote it.
"""

from __future__ import annotations

import json
import urllib.parse

from .executor import Executor
from .jsonval import Json, as_obj


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

    def count(self, run: str, stream: str, start_ms: int, end_ms: int) -> int | None:
        status, body = self._query(f"{_stream_filter(run, stream)} | stats count() n", start_ms, end_ms)
        if status != 200:
            return None
        for line in body.splitlines():
            if line.strip():
                try:
                    return int(str(as_obj(json.loads(line)).get("n", "0")))
                except ValueError:
                    return None
        return 0

    def export(self, run: str, start_ms: int, end_ms: int) -> tuple[list[Json], list[str]]:
        """The run's records, and the lines that did not parse back into one."""
        status, body = self._query(_stream_filter(run, None), start_ms, end_ms)
        if status != 200:
            return [], [f"VictoriaLogs export failed: HTTP {status}: {body[:200]}"]
        records: list[Json] = []
        bad: list[str] = []
        for line in body.splitlines():
            if not line.strip():
                continue
            try:
                row = as_obj(json.loads(line))
                records.append(as_obj(json.loads(str(row.get("_msg", "")))))
            except ValueError:
                bad.append(line[:200])
        return records, bad
