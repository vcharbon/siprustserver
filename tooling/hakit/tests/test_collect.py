"""The CDR export after the drain (ADR-0049 §3.8): complete once every CDR the
workers wrote was consumed and the shipped observations stopped growing."""

from __future__ import annotations

import json
import urllib.parse
from pathlib import Path
from typing import Any

from kit import T0

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
    """Workers wrote 10 CDRs; the consumer has consumed `consumed(now)` of them."""

    def __init__(self, clock: FakeClock, consumed_after_s: float | None, unread: float = 0, shipping_until_s: float = 0) -> None:
        self.clock = clock
        self.consumed_after_s = consumed_after_s
        self.unread = unread
        self.shipping_until_s = shipping_until_s

    def rule(self, c: Call) -> HttpResult | None:
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query).get("query", [""])[0]
        elapsed = (self.clock.now_ms() - T0) / 1000
        if "b2bua_cdr_written_total" in q:
            return vector(10)
        if "cdr_consumed_total" in q:
            done = self.consumed_after_s is not None and elapsed >= self.consumed_after_s
            return vector(10 if done else 8)
        if "cdr_observations_unread_total" in q:
            return vector(self.unread)
        if "count() cdrs" in q:
            shipped = 10 + int(min(elapsed, self.shipping_until_s))
            return HttpResult(200, f'{{"cdrs":"{shipped}"}}\n')
        if "/select/logsql/query" in c.url:
            line = {"_msg": json.dumps({"call_id": "1-run-x@h", "end_ms": 1}), "source": "cdr-consumer"}
            return HttpResult(200, json.dumps(line) + "\n")
        return None


def collect(tmp_path: Path, chain_after_s: float | None, unread: float = 0, shipping_until_s: float = 0) -> dict[str, Any]:
    clock = FakeClock(T0)
    fake = FakeExecutor([Chain(clock, chain_after_s, unread, shipping_until_s).rule])
    found = collect_cdrs(VictoriaLogs(fake, "vl"), VictoriaMetrics(fake, "vm"), LAB, clock, "run-x", tmp_path, T0 - 600_000)
    assert found == []
    assert [json.loads(line)["call_id"] for line in (tmp_path / "cdrs.jsonl").read_text().splitlines()] == ["1-run-x@h"]
    doc = json.loads((tmp_path / "cdrs-collect.json").read_text())
    doc["waited_s"] = (clock.now_ms() - T0) / 1000
    return doc


def test_the_export_waits_until_every_written_cdr_was_consumed(tmp_path: Path) -> None:
    doc = collect(tmp_path, 120)
    assert (doc["complete"], doc["written"], doc["consumed"], doc["unread"]) == (True, 10, 10, 0)
    assert 120 <= doc["waited_s"] < 140  # long enough for a broker thaw


def test_cdrs_still_owed_at_the_cap_leave_the_export_incomplete(tmp_path: Path) -> None:
    doc = collect(tmp_path, None, unread=2)
    assert (doc["complete"], doc["written"], doc["consumed"], doc["unread"]) == (False, 10, 8, 2)
    assert doc["waited_s"] >= MAX_WAIT_S >= 300


def test_the_export_waits_for_the_shipped_observations_to_stop_growing(tmp_path: Path) -> None:
    doc = collect(tmp_path, 0, shipping_until_s=150)
    assert doc["complete"] is True and 160 <= doc["waited_s"] < 170
