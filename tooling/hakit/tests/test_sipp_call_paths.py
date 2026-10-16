"""The base UAC's in-dialog and abuse paths, each against a SIPp peer callee on loopback.

Every pairing passes the keys that select a path (re-INVITEs, the far end's BYE
as the planned end, a ghost or a flood) and checks the one schema-valid record
the call left, that the UAC exited 0 and that the peer, which fails on any
missing answer, ACK or BYE, exited 0. A pairing against the base UAS runs the
two base scenarios together. Needs `sipp` on PATH.
"""

from __future__ import annotations

import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from sipp_pairing import SIPP, UAS, Pairing, run_pair

from hakit import jsonval as jv
from hakit.schemas import SchemaSet


@dataclass(frozen=True)
class Case:
    id: str
    peer: str | Path
    ending: str
    verdict: str
    keys: dict[str, str] = field(default_factory=lambda: {})
    peer_keys: dict[str, str] = field(default_factory=lambda: {})
    failures: list[str] = field(default_factory=lambda: [])
    ruri_user: str = "service"
    calls: int = 1
    rate: int = 10
    phase: str = "released"


UAS_FAST = {"ring_ms": "200", "gap_ms": "300"}
CASES = [
    Case("far-end-bye-release", "peer-bye-mid-hold.xml", "answered", "far_end_release", {"far_end_bye": "release"}),
    Case("reinvites-vs-uas", UAS, "answered", "answered", {"reinvites": "2", "hold_ms": "3000"}, UAS_FAST),
    Case("options-crosses-reinvite", "peer-uas-options-crosses-reinvite.xml", "answered", "answered", {"reinvites": "2", "hold_ms": "6000"}),
    Case("options-mid-hold", "peer-uas-options-mid-hold.xml", "answered", "answered", {"hold_ms": "2000"}),
    Case("options-crosses-bye", "peer-uas-options-crosses-bye.xml", "answered", "answered", {"hold_ms": "2000"}),
    Case("reinvite-488", "peer-reinvite-488.xml", "answered", "answered", {"reinvites": "1"}, {}, ["dialog_request"]),
    Case("reinvite-481", "peer-reinvite-481.xml", "cut", "reinvite_481", {"reinvites": "1"}, {}, ["dialog_request"]),
    Case("reinvite-408", "peer-reinvite-408.xml", "cut", "reinvite_408", {"reinvites": "1"}, {}, ["dialog_request"]),
    Case("reinvite-no-final", "peer-reinvite-silent.xml", "cut", "reinvite_no_final", {"reinvites": "1"}, {}, ["dialog_request"]),
    Case(
        "far-reinvites-release",
        UAS,
        "answered",
        "far_end_release",
        {"far_end_bye": "release", "hold_ms": "5000"},
        UAS_FAST,
        ruri_user="reinvite",
    ),
    Case("far-reinvites-cut", UAS, "cut", "bye_from_far_end", {"hold_ms": "5000"}, UAS_FAST, ruri_user="reinvite"),
    Case("far-reinvite-unacked", "peer-reinvite-unacked.xml", "cut", "far_reinvite_no_ack", {"hold_ms": "15000"}, {}, ["protocol"]),
    Case("ghost-released", "peer-ghost-bye.xml", "answered", "ghost_released", {"abuse": "ghost", "hold_ms": "15000"}),
    Case("ghost-never-released", "peer-answer-quiet.xml", "stranded", "ghost_never_released", {"abuse": "ghost"}, phase="answered"),
    Case("options-flood", "peer-flood-target.xml", "answered", "answered", {"abuse": "options_flood", "flood_count": "5", "flood_gap_ms": "100"}),
    Case("reinvite-flood", "peer-flood-target.xml", "answered", "answered", {"abuse": "reinvite_flood", "flood_count": "5", "flood_gap_ms": "100"}),
    Case(
        "flood-cut-by-far-end",
        "peer-bye-mid-hold.xml",
        "answered",
        "far_end_release",
        {"abuse": "options_flood", "flood_count": "50", "flood_gap_ms": "1000"},
    ),
    Case("plain-vs-uas", UAS, "answered", "answered", {}, UAS_FAST),
    Case("setup-timeout-vs-uas", UAS, "timeout", "setup_timeout", {"setup_ms": "1000"}, {"ring_ms": "5000"}, phase="provisional"),
]
RACE = Case("options-on-hold-deadline", "peer-uas-options-on-hold-deadline.xml", "answered", "answered", {"hold_ms": "2000"}, calls=300, rate=50)


@pytest.fixture(scope="module")
def pairings() -> dict[str, Pairing]:
    assert SIPP is not None, "sipp is not on PATH: the SIPp record checks are part of the gate"
    cases = [*CASES, RACE]
    with tempfile.TemporaryDirectory(prefix="hakit-call-") as tmp, ThreadPoolExecutor(len(cases)) as pool:
        work = Path(tmp)
        futures = {
            c.id: pool.submit(run_pair, c.peer, work, c.calls, c.rate, keys=c.keys, peer_keys=c.peer_keys, ruri_user=c.ruri_user)
            for c in cases
        }
        return {k: f.result() for k, f in futures.items()}


@pytest.mark.parametrize("case", CASES, ids=[c.id for c in CASES])
def test_each_path_writes_its_record(case: Case, pairings: dict[str, Pairing]) -> None:
    p = pairings[case.id]
    assert p.uac_rc == 0, f"{case.id}: sipp exited {p.uac_rc}"
    assert p.peer_rc == 0, f"{case.id}: the peer exited {p.peer_rc}"
    assert len(p.records) == 1, f"{case.id}: {len(p.records)} records"
    r = p.records[0]
    assert SchemaSet.load().errors("call-outcome.v1", r) == []
    assert (r["ending"], r["producer_verdict"], r["phase_reached"]) == (case.ending, case.verdict, case.phase)
    assert [f["kind"] for f in jv.objs(r, "failures")] == case.failures


def test_options_on_the_hold_deadline_never_cost_a_call(pairings: dict[str, Pairing]) -> None:
    """Across 300 calls the peer's OPTIONS lands before, on and across the
    caller's BYE: every call is answered, every OPTIONS answered."""
    p = pairings[RACE.id]
    assert (p.uac_rc, p.peer_rc) == (0, 0)
    assert len(p.records) == RACE.calls
    assert {str(r["ending"]) for r in p.records} == {"answered"}
