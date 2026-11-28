"""The CDR export after the drain (ADR-0049 §3.8): complete once the run's
distinct shipped observations cover every CDR the workers wrote and have
stopped growing."""

from __future__ import annotations

import json
import urllib.parse
from pathlib import Path
from typing import Any

from kit import T0, Samples, logsql_holds, vm_export

from hakit.clock import FakeClock
from hakit.collect import MAX_WAIT_S, collect_cdrs
from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.lab import lab_from_env
from hakit.vlogs import VictoriaLogs
from hakit.vm import VictoriaMetrics

LAB = lab_from_env({})
STARTED = T0 - 600_000
# A worker's counter through the run: 5 CDRs of an earlier run before the
# start, 10 of this run after it.
ONE_WORKER: list[Samples] = [[(STARTED - 30_000, 5), (STARTED + 15_000, 9), (STARTED + 30_000, 15)]]


def counter(name: str, pod: str, samples: Samples) -> tuple[dict[str, str], Samples]:
    return {"__name__": name, "pod": pod}, samples


class Chain:
    """Workers wrote 10 CDRs, their counter series `written`. The run's
    distinct observations reach 10 at `observed_after_s` (8 before, 8 forever
    when None) and keep growing until `shipping_until_s`; the consumer's
    counter follows them unless `counter_lost` (a consumer killed between two
    scrapes loses what it counted since the last one); another run's
    observations are never in the run's count."""

    def __init__(
        self,
        clock: FakeClock,
        observed_after_s: float | None,
        unread: float = 0,
        shipping_until_s: float = 0,
        counter_lost: bool = False,
        written: list[Samples] = ONE_WORKER,
    ) -> None:
        self.clock = clock
        self.observed_after_s = observed_after_s
        self.unread = unread
        self.shipping_until_s = shipping_until_s
        self.counter_lost = counter_lost
        self.written = written

    def observed(self) -> int:
        elapsed = (self.clock.now_ms() - T0) / 1000
        done = self.observed_after_s is not None and elapsed >= self.observed_after_s
        return (10 if done else 8) + int(min(elapsed, self.shipping_until_s))

    def rule(self, c: Call) -> HttpResult | None:
        if "/api/v1/export" in c.url:
            now = self.clock.now_ms()
            consumed = 8 if self.counter_lost else min(self.observed(), 10)
            return HttpResult(200, vm_export(c.url, [
                *(counter("b2bua_cdr_written_total", f"w-{i}", s) for i, s in enumerate(self.written)),
                counter("cdr_consumed_total", "c-0", [(STARTED - 30_000, 40), (now, 40 + consumed)]),
                counter("cdr_observations_unread_total", "c-0", [(STARTED - 30_000, 3), (now, 3 + self.unread)]),
            ]))
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query).get("query", [""])[0]
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
    found = collect_cdrs(VictoriaLogs(fake, "vl"), VictoriaMetrics(fake, "vm"), LAB, chain.clock, "run-x", tmp_path, STARTED)
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


def test_a_rollout_that_replaces_a_workers_series_counts_the_old_series_and_the_new(tmp_path: Path) -> None:
    # The old pod counted 6 of the run's CDRs before its series ended (its
    # staleness marker last); its replacement's series starts at 0 and counts 4.
    old: Samples = [(STARTED - 30_000, 100), (STARTED + 15_000, 104), (STARTED + 30_000, 106), (STARTED + 45_000, None)]
    new: Samples = [(STARTED + 60_000, 4), (STARTED + 75_000, 4)]
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[old, new]))
    assert (doc["complete"], doc["written"], doc["observed"]) == (True, 10, 10)
    assert 30 <= doc["waited_s"] < 50


def test_a_worker_restarted_in_place_counts_after_its_reset(tmp_path: Path) -> None:
    restarted: Samples = [(STARTED - 30_000, 50), (STARTED + 15_000, 57), (STARTED + 30_000, 1), (STARTED + 45_000, 3)]
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[restarted]))
    assert (doc["complete"], doc["written"]) == (True, 10)


def test_cdrs_written_before_the_run_started_are_not_owed_to_it(tmp_path: Path) -> None:
    # An earlier run's last CDRs land in the minute before the start.
    tail: Samples = [(STARTED - 50_000, 0), (STARTED - 35_000, 300), (STARTED - 20_000, 500), (STARTED + 15_000, 510)]
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[tail]))
    assert (doc["complete"], doc["written"]) == (True, 10)
