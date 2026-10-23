"""`hakit mark`: a supervising workflow's state as series and annotations."""

from __future__ import annotations

import json

import pytest

from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.marks import PHASES, lines, mark

AT = 1_800_000_000_000


def test_a_phase_writes_every_phase_so_the_previous_one_goes_dark() -> None:
    got = lines("phase", ["r1", "running"], AT)
    assert len(got) == len(PHASES) + 1
    assert f'sip_soak_phase{{run="r1",phase="running"}} 1 {AT}' in got
    assert f'sip_soak_phase{{run="r1",phase="bringup"}} 0 {AT}' in got
    assert got[-1] == f'sip_soak_up{{run="r1"}} 1 {AT}'
    with pytest.raises(ValueError):
        lines("phase", ["r1", "sleeping"], AT)


def test_progress_and_findings_are_series() -> None:
    assert lines("progress", ["r1", "3", "31"], AT) == [
        f'sip_soak_events_seen{{run="r1"}} 3 {AT}',
        f'sip_soak_events_expected{{run="r1"}} 31 {AT}',
    ]
    assert lines("finding", ["r1", "ghost-calls", "major"], AT) == [
        f'sip_soak_finding{{run="r1",signature="ghost-calls",severity="major"}} 1 {AT}'
    ]


def test_a_mark_annotates_grafana_and_a_refusal_does_not_fail_it() -> None:
    def refuse(c: Call) -> HttpResult | None:
        return HttpResult(500, "no") if c.kind == "http" else None

    fake = FakeExecutor([refuse])
    assert mark(fake, "http://vm", "http://grafana", "finding", ["r1", "sig", "minor"], AT) == 0
    push, note = fake.calls
    assert push.url == "http://vm/api/v1/import/prometheus"
    assert note.url == "http://grafana/api/annotations"
    assert json.loads(note.body or b"{}") == {"text": "finding [minor] sig", "tags": ["soak", "finding"], "time": AT}
    assert mark(fake, "http://vm", "http://grafana", "phase", ["only-a-run"], AT) == 2
