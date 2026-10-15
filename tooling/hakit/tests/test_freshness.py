"""Freshness of the series a run reads live (ADR-0049 §6.6)."""

from __future__ import annotations

import urllib.parse

from kit import T0

from hakit.clock import FakeClock
from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.freshness import Freshness
from hakit.jsonval import Json
from hakit.vlogs import VictoriaLogs
from hakit.vm import VictoriaMetrics


class Lab:
    """Records that stop arriving at `records_stop_ms`, a series whose newest sample
    stops at `series_stop_ms`."""

    def __init__(self, clock: FakeClock, records_stop_ms: int, series_stop_ms: int) -> None:
        self.clock = clock
        self.records_stop_ms = records_stop_ms
        self.series_stop_ms = series_stop_ms

    def rule(self, c: Call) -> HttpResult | None:
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query).get("query", [""])[0]
        now = self.clock.now_ms()
        if "count() live" in q:
            return HttpResult(200, f'{{"live":"{(min(now, self.records_stop_ms) - T0) // 1000}"}}\n')
        if "timestamp(" in q:
            newest = min(now, self.series_stop_ms) / 1000
            return HttpResult(200, f'{{"status":"success","data":{{"result":[{{"value":[0,"{newest}"]}}]}}}}')
        return None


def watch(records_stop_ms: int, series_stop_ms: int, until_ms: int, cps: float = 10) -> list[Json]:
    clock = FakeClock(T0)
    fake = FakeExecutor([Lab(clock, records_stop_ms, series_stop_ms).rule])
    f = Freshness("r", VictoriaMetrics(fake, "vm"), VictoriaLogs(fake, "vl"), clock, [("basic", cps, 30)], ["sut_calls"], T0)
    found: list[Json] = []
    while clock.now_ms() < until_ms:
        clock.advance_ms(1000)
        found += f.tick()
    return found


def test_fresh_series_raise_nothing() -> None:
    assert watch(T0 + 10_000_000, T0 + 10_000_000, T0 + 600_000) == []


def test_records_that_stop_growing_are_stale_once_from_the_last_growth_seen() -> None:
    found = watch(T0 + 120_000, T0 + 10_000_000, T0 + 600_000)
    # Checks run every 15 s; the last one that saw growth was at +120 s.
    assert found == [
        {"kind": "series_stale", "at_ms": T0 + 120_000, "detail": "the records of stream basic stopped updating", "stream": "basic"}
    ]


def test_a_series_whose_newest_sample_ages_three_intervals_is_stale() -> None:
    found = watch(T0 + 10_000_000, T0 + 200_000, T0 + 600_000)
    assert found == [{"kind": "series_stale", "at_ms": T0 + 200_000, "detail": "the series sut_calls stopped updating"}]


def test_no_record_before_the_first_calls_can_end_is_no_staleness() -> None:
    # Records never arrive: stale once the first calls could have ended (30 s) plus
    # more than 45 s, at the check of +90 s.
    found = watch(T0, T0 + 10_000_000, T0 + 89_000)
    assert found == []
    found = watch(T0, T0 + 10_000_000, T0 + 90_000)
    assert [f["at_ms"] for f in found] == [T0 + 30_000]


def test_a_slow_stream_waits_five_of_its_calls() -> None:
    # At 0.05 cps five calls take 100 s, longer than three 15 s intervals.
    assert watch(T0 + 120_000, T0 + 10_000_000, T0 + 210_000, cps=0.05) == []
    assert len(watch(T0 + 120_000, T0 + 10_000_000, T0 + 240_000, cps=0.05)) == 1
