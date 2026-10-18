"""The VictoriaLogs export: a long range is read one bounded query at a time."""

from __future__ import annotations

import json
import urllib.parse

from kit import T0, logsql_holds

from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.vlogs import SLICE_MS, VictoriaLogs

HOURS_4 = 4 * 3_600_000


def query(c: Call) -> str:
    return urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query)["query"][0]


def stored(at_ms: list[int], failing_at_ms: int | None = None) -> FakeExecutor:
    """One record per instant of `at_ms`; the slice holding `failing_at_ms` fails."""

    def rule(c: Call) -> HttpResult | None:
        if failing_at_ms is not None and logsql_holds(c.url, failing_at_ms):
            return HttpResult(0, "IncompleteRead(255200893 bytes read)")
        rows = [{"_msg": json.dumps({"call_id": f"c{t}", "ended_at_ms": t})} for t in at_ms if logsql_holds(c.url, t)]
        return HttpResult(200, "".join(json.dumps(r) + "\n" for r in rows))

    return FakeExecutor([rule])


def test_a_four_hour_export_is_one_query_per_slice_each_record_once() -> None:
    at = list(range(T0, T0 + HOURS_4, 7_000))
    fake = stored(at)
    bad: list[str] = []
    got = [r["call_id"] for r in VictoriaLogs(fake, "vl").export("run-x", T0, T0 + HOURS_4, bad)]
    assert got == [f"c{t}" for t in at] and bad == []
    queries = [query(c) for c in fake.calls]
    assert len(queries) == HOURS_4 // SLICE_MS
    assert all(q == '_stream:{run="run-x"} | fields _msg' for q in queries)


def test_a_failed_slice_is_reported_and_the_others_kept() -> None:
    at = [T0 + 1_000, T0 + SLICE_MS + 1_000, T0 + 2 * SLICE_MS + 1_000]
    bad: list[str] = []
    got = [r["call_id"] for r in VictoriaLogs(stored(at, at[1]), "vl").export("run-x", T0, T0 + 3 * SLICE_MS, bad)]
    assert got == [f"c{at[0]}", f"c{at[2]}"]
    assert len(bad) == 1 and f"[{T0 + SLICE_MS}, {T0 + 2 * SLICE_MS})" in bad[0] and "HTTP 0" in bad[0]


def test_the_cdr_export_asks_only_for_the_run() -> None:
    fake = FakeExecutor([lambda c: HttpResult(200, "")])
    assert list(VictoriaLogs(fake, "vl").export_cdrs("run-x", T0, T0 + 1_000, [])) == []
    assert [query(c) for c in fake.calls] == ['_stream:{source="cdr-consumer"} "run-x" | fields _msg']


def test_the_distinct_cdr_count_is_counted_by_hash() -> None:
    fake = FakeExecutor([lambda c: HttpResult(200, '{"cdrs":"7"}\n')])
    assert VictoriaLogs(fake, "vl").cdr_count("run-x", T0, T0 + HOURS_4) == 7
    assert [query(c) for c in fake.calls] == ['_stream:{source="cdr-consumer"} "run-x" | stats count_uniq_hash(_msg) cdrs']
