"""The CDR export after the drain (ADR-0049 §3.8): complete once the run's
distinct shipped observations cover every CDR the workers wrote and have
stopped growing."""

from __future__ import annotations

import json
import urllib.parse
from pathlib import Path
from typing import Any

from kit import T0, logsql_holds

from hakit.clock import FakeClock
from hakit.collect import MAX_WAIT_S, collect_cdrs
from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.lab import lab_from_env
from hakit.vlogs import VictoriaLogs
from hakit.vm import VictoriaMetrics

LAB = lab_from_env({})


def vector(v: float) -> HttpResult:
    return HttpResult(200, json.dumps({"status": "success", "data": {"result": [{"metric": {}, "value": [0, str(v)]}]}}))


class Chain:
    """Workers wrote 10 CDRs. The run's distinct observations reach 10 at
    `observed_after_s` (8 before, 8 forever when None) and keep growing until
    `shipping_until_s`; the consumer's counter follows them unless
    `counter_lost` (a consumer killed between two scrapes loses what it
    counted since the last one); another run's observations are never in the
    run's count."""

    def __init__(
        self,
        clock: FakeClock,
        observed_after_s: float | None,
        unread: float = 0,
        shipping_until_s: float = 0,
        counter_lost: bool = False,
    ) -> None:
        self.clock = clock
        self.observed_after_s = observed_after_s
        self.unread = unread
        self.shipping_until_s = shipping_until_s
        self.counter_lost = counter_lost

    def observed(self) -> int:
        elapsed = (self.clock.now_ms() - T0) / 1000
        done = self.observed_after_s is not None and elapsed >= self.observed_after_s
        return (10 if done else 8) + int(min(elapsed, self.shipping_until_s))

    def rule(self, c: Call) -> HttpResult | None:
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query).get("query", [""])[0]
        if "b2bua_cdr_written_total" in q:
            return vector(10)
        if "cdr_consumed_total" in q:
            return vector(8 if self.counter_lost else min(self.observed(), 10))
        if "cdr_observations_unread_total" in q:
            return vector(self.unread)
        if "stats count_uniq_hash(_msg) cdrs" in q:
            return HttpResult(200, f'{{"cdrs":"{self.observed() if "run-x" in q else 1000}"}}\n')
        if "/select/logsql/query" in c.url:
            if not logsql_holds(c.url, T0):
                return HttpResult(200, "")
            line = {"_msg": json.dumps({"call_id": "1-run-x@h", "end_ms": 1}), "source": "cdr-consumer"}
            return HttpResult(200, json.dumps(line) + "\n")
        return None


def collect(tmp_path: Path, chain: Chain) -> dict[str, Any]:
    fake = FakeExecutor([chain.rule])
    found = collect_cdrs(VictoriaLogs(fake, "vl"), VictoriaMetrics(fake, "vm"), LAB, chain.clock, "run-x", tmp_path, T0 - 600_000)
    assert found == []
    assert [json.loads(line)["call_id"] for line in (tmp_path / "cdrs.jsonl").read_text().splitlines()] == ["1-run-x@h"]
    doc = json.loads((tmp_path / "cdrs-collect.json").read_text())
    doc["waited_s"] = (chain.clock.now_ms() - T0) / 1000
    return doc


def test_the_export_waits_until_every_written_cdr_was_observed(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 120))
    assert (doc["complete"], doc["written"], doc["observed"], doc["consumed"], doc["unread"]) == (True, 10, 10, 10, 0)
    assert 120 <= doc["waited_s"] < 140  # long enough for a broker thaw


def test_cdrs_still_owed_at_the_cap_leave_the_export_incomplete(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), None, unread=2))
    assert (doc["complete"], doc["written"], doc["observed"], doc["unread"]) == (False, 10, 8, 2)
    assert doc["waited_s"] >= MAX_WAIT_S >= 300


def test_the_export_waits_for_the_shipped_observations_to_stop_growing(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 0, shipping_until_s=150))
    assert doc["complete"] is True and 160 <= doc["waited_s"] < 170


def test_a_consumer_counter_reset_by_a_kill_does_not_hold_the_export(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, counter_lost=True))
    assert (doc["complete"], doc["written"], doc["observed"], doc["consumed"]) == (True, 10, 10, 8)
    assert 30 <= doc["waited_s"] < 50
