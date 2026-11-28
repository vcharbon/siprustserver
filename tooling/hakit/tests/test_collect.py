"""The CDR export after the drain (ADR-0049 §3.8): complete once the run's
distinct shipped observations cover every CDR the workers wrote, have stopped
growing, and the broker's queue reads empty after that; with no queue reading,
only when no written series ended or reset."""

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
WRITTEN = 10
SCRAPE_MS = 15_000
# A worker's counter through the run: 5 CDRs of an earlier run before the
# start, 10 of this run after it.
ONE_WORKER: list[Samples] = [[(STARTED - 30_000, 5), (STARTED + 15_000, 9), (STARTED + 30_000, 15)]]
# A worker killed between two scrapes: its last scrape saw 4 of the 6 CDRs it
# wrote; its replacement wrote the other 4.
KILLED: Samples = [(STARTED - 30_000, 100), (STARTED + 15_000, 104), (STARTED + 30_000, None)]
REPLACEMENT: Samples = [(STARTED + 60_000, 4), (STARTED + 75_000, 4)]
# A worker rolled out after its last scrape counted its 6: its series ends
# with a staleness marker, its replacement's starts at 0 and counts 4.
ROLLED_OUT: Samples = [(STARTED - 30_000, 100), (STARTED + 15_000, 104), (STARTED + 30_000, 106), (STARTED + 45_000, None)]
# A worker restarted in place: its series drops to the counts of the new
# process, 7 before the restart and 3 after.
RESTARTED: Samples = [(STARTED - 30_000, 50), (STARTED + 15_000, 57), (STARTED + 30_000, 1), (STARTED + 45_000, 3)]
# An earlier run's last CDRs land in the minute before the start.
EARLIER_RUN_TAIL: Samples = [(STARTED - 50_000, 0), (STARTED - 35_000, 300), (STARTED - 20_000, 500), (STARTED + 15_000, 510)]
# The broker's queue holding 3 CDRs past the cap, scraped every 15 s.
QUEUE_HELD: Samples = [(t, 3.0) for t in range(T0, T0 + 2 * MAX_WAIT_S * 1000, SCRAPE_MS)]
# A queue read empty once, a minute before the export began, and never since.
QUEUE_EMPTY_BEFORE_THE_EXPORT: Samples = [(T0 - 60_000, 0.0)]
# A broker instance replaced while it held 41 CDRs (its series ends with a
# staleness marker at +60 s), and its successor reading an empty queue.
BROKER_REPLACED: list[Samples] = [
    [(T0 + 30_000, 41.0), (T0 + 45_000, 41.0), (T0 + 60_000, None)],
    [(t, 0.0) for t in range(T0 + 30_000, T0 + 2 * MAX_WAIT_S * 1000, SCRAPE_MS)],
]


def counter(name: str, pod: str, samples: Samples) -> tuple[dict[str, str], Samples]:
    return {"__name__": name, "pod": pod}, samples


class Chain:
    """Workers wrote 10 CDRs, their counter series `written`. The run's
    distinct observations reach 10 at `observed_after_s` (8 before, 8 forever
    when None) and keep growing until `shipping_until_s`; the broker's queue
    holds the CDRs not observed yet, scraped every 15 s, unless `queues` gives
    the gauge's series or `broker_unscraped`; the consumer's counter follows
    the observations unless `counter_lost` (a consumer killed between two
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
        broker_unscraped: bool = False,
        queues: list[Samples] | None = None,
    ) -> None:
        self.clock = clock
        self.observed_after_s = observed_after_s
        self.unread = unread
        self.shipping_until_s = shipping_until_s
        self.counter_lost = counter_lost
        self.written = written
        self.broker_unscraped = broker_unscraped
        self.queues = queues

    def observed(self, at_ms: int) -> int:
        elapsed = (at_ms - T0) / 1000
        done = self.observed_after_s is not None and elapsed >= self.observed_after_s
        return (WRITTEN if done else 8) + int(min(elapsed, self.shipping_until_s))

    def queue_series(self, now_ms: int) -> list[Samples]:
        if self.broker_unscraped:
            return []
        if self.queues is not None:
            return self.queues
        return [[(t, float(max(0, WRITTEN - self.observed(t)))) for t in range(T0, now_ms + 1, SCRAPE_MS)]]

    def rule(self, c: Call) -> HttpResult | None:
        now = self.clock.now_ms()
        if "/api/v1/export" in c.url:
            consumed = 8 if self.counter_lost else min(self.observed(now), WRITTEN)
            return HttpResult(200, vm_export(c.url, [
                *(counter("b2bua_cdr_written_total", f"w-{i}", s) for i, s in enumerate(self.written)),
                counter("cdr_consumed_total", "c-0", [(STARTED - 30_000, 40), (now, 40 + consumed)]),
                counter("cdr_observations_unread_total", "c-0", [(STARTED - 30_000, 3), (now, 3 + self.unread)]),
                *(counter("rabbitmq_queue_messages", f"b-{i}", s) for i, s in enumerate(self.queue_series(now))),
            ]))
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query).get("query", [""])[0]
        if "stats count_uniq_hash(_msg) cdrs" in q:
            return HttpResult(200, f'{{"cdrs":"{self.observed(now) if "run-x" in q else 1000}"}}\n')
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


def test_the_export_waits_through_a_late_broker_thaw_until_every_written_cdr_was_observed(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 120))
    assert (doc["complete"], doc["written"], doc["observed"], doc["consumed"], doc["unread"]) == (True, 10, 10, 10, 0)
    assert (doc["written_lossy"], doc["queued"]) == (False, 0)
    assert 120 <= doc["waited_s"] < 140


def test_cdrs_still_owed_at_the_cap_leave_the_export_incomplete(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), None, unread=2))
    assert (doc["complete"], doc["written"], doc["observed"], doc["unread"], doc["queued"]) == (False, 10, 8, 2, 2)
    assert doc["waited_s"] >= MAX_WAIT_S >= 300


def test_the_export_waits_for_the_shipped_observations_to_stop_growing(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 0, shipping_until_s=150))
    assert doc["complete"] is True and 160 <= doc["waited_s"] < 170


def test_a_consumer_counter_reset_by_a_kill_does_not_hold_the_export(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, counter_lost=True))
    assert (doc["complete"], doc["written"], doc["observed"], doc["consumed"]) == (True, 10, 10, 8)
    assert 30 <= doc["waited_s"] < 50


def test_cdrs_a_killed_worker_wrote_after_its_last_scrape_still_queued_leave_the_export_incomplete(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), None, written=[KILLED, REPLACEMENT]))
    assert (doc["complete"], doc["written"], doc["written_lossy"], doc["observed"], doc["queued"]) == (False, 8, True, 8, 2)
    assert doc["waited_s"] >= MAX_WAIT_S


def test_a_killed_workers_unscraped_cdrs_hold_the_export_until_the_queue_reads_empty(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[KILLED, REPLACEMENT]))
    assert (doc["complete"], doc["written"], doc["observed"], doc["queued"]) == (True, 8, 10, 0)
    assert 40 <= doc["waited_s"] < 60


def test_a_killed_worker_with_no_queue_reading_leaves_the_export_incomplete(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 0, written=[KILLED, REPLACEMENT], broker_unscraped=True))
    assert (doc["complete"], doc["written_lossy"], doc["observed"], doc["queued"]) == (False, True, 10, None)


def test_a_queue_read_empty_only_before_the_observations_went_quiet_is_no_reading(tmp_path: Path) -> None:
    chain = Chain(FakeClock(T0), 30, written=[KILLED, REPLACEMENT], queues=[QUEUE_EMPTY_BEFORE_THE_EXPORT])
    doc = collect(tmp_path, chain)
    assert (doc["complete"], doc["written_lossy"], doc["observed"], doc["queued"]) == (False, True, 10, None)


def test_a_replaced_broker_instance_left_holding_cdrs_does_not_hold_the_export(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[KILLED, REPLACEMENT], queues=BROKER_REPLACED))
    assert (doc["complete"], doc["observed"], doc["queued"]) == (True, 10, 0)
    assert 60 <= doc["waited_s"] < 80


def test_a_queue_still_holding_cdrs_holds_the_export_though_no_written_series_ended(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, queues=[QUEUE_HELD]))
    assert (doc["complete"], doc["written_lossy"], doc["observed"], doc["queued"]) == (False, False, 10, 3)


def test_with_no_written_series_ended_or_reset_the_counts_alone_complete_the_export(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, broker_unscraped=True))
    assert (doc["complete"], doc["written_lossy"], doc["queued"]) == (True, False, None)
    assert 30 <= doc["waited_s"] < 50


def test_a_rollout_that_replaces_a_workers_series_counts_the_old_series_and_the_new(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[ROLLED_OUT, REPLACEMENT]))
    assert (doc["complete"], doc["written"], doc["written_lossy"], doc["observed"]) == (True, 10, True, 10)
    assert 30 <= doc["waited_s"] < 60


def test_a_worker_restarted_in_place_counts_after_its_reset(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[RESTARTED]))
    assert (doc["complete"], doc["written"], doc["written_lossy"]) == (True, 10, True)


def test_cdrs_written_before_the_run_started_are_not_owed_to_it(tmp_path: Path) -> None:
    doc = collect(tmp_path, Chain(FakeClock(T0), 30, written=[EARLIER_RUN_TAIL]))
    assert (doc["complete"], doc["written"], doc["written_lossy"]) == (True, 10, False)
