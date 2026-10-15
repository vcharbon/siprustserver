"""The base UAC scenario writes one schema-valid call-outcome record at every exit.

Each pairing runs the scenario against a SIPp peer on loopback that drives one
exit (answer, reject, ring forever, silence, a late answer, a 2xx crossing the
CANCEL, far-end BYE, 481 or no final to the BYE, an in-dialog request the
caller cannot serve, a 2xx retransmitted after the ACK) and checks the record
the call left and that the peer, which fails on any missing ACK, BYE or
answer, exits 0. Needs `sipp` on PATH: without it the tests fail.
"""

from __future__ import annotations

import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from sipp_pairing import SIPP, Pairing, run_pair

from hakit import jsonval as jv
from hakit.schemas import SchemaSet


@dataclass(frozen=True)
class Case:
    peer: str
    ending: str
    final_code: int | None
    reason: str | None
    phase: str
    failures: list[str] = field(default_factory=lambda: [])
    early_media: bool = False
    provisional: bool = True


CASES = [
    Case("peer-answer.xml", "answered", 200, None, "released"),
    Case("peer-early-media.xml", "answered", 200, None, "released", early_media=True),
    Case("peer-reject-486.xml", "rejected", 486, "486", "provisional"),
    Case("peer-reject-503-reason.xml", "rejected", 503, "Q.850;cause=34", "invited", provisional=False),
    Case("peer-reason-list.xml", "rejected", 503, "SIP;cause=503", "invited", provisional=False),
    Case("peer-ring-no-answer.xml", "timeout", None, "timeout", "provisional"),
    Case("peer-cancel-crossing.xml", "timeout", None, "timeout", "provisional"),
    Case("peer-silent.xml", "timeout", None, "timeout", "invited", provisional=False),
    Case("peer-late-answer.xml", "timeout", None, "timeout", "invited", provisional=False),
    Case("peer-tick.xml", "answered", 200, None, "released"),
    Case("peer-2xx-twice.xml", "answered", 200, None, "released"),
    Case("peer-ack-lost.xml", "answered", 200, None, "released"),
    Case("peer-ack-lost-at-bye.xml", "answered", 200, None, "released"),
    Case("peer-bye-mid-hold.xml", "cut", 200, None, "released", provisional=False),
    Case("peer-bye-481.xml", "cut", 200, None, "released", failures=["dialog_request"]),
    Case("peer-bye-no-final.xml", "stranded", 200, None, "answered", failures=["dialog_request"]),
    Case("peer-info-mid-hold.xml", "failed", 200, None, "released", failures=["protocol"], provisional=False),
    Case("peer-final-after-2xx.xml", "answered", 200, None, "released", failures=["protocol"]),
]
# Peers that never complete their side by design.
PEER_MAY_FAIL = {"peer-silent.xml", "peer-bye-no-final.xml"}
BULK_PEER, BULK_CALLS = "peer-tick.xml", 150


@pytest.fixture(scope="module")
def pairings() -> dict[str, Pairing]:
    assert SIPP is not None, "sipp is not on PATH: the SIPp record checks are part of the gate"
    with tempfile.TemporaryDirectory(prefix="hakit-sipp-") as tmp, ThreadPoolExecutor(len(CASES) + 1) as pool:
        work = Path(tmp)
        futures = {c.peer: pool.submit(run_pair, c.peer, work) for c in CASES}
        futures["bulk"] = pool.submit(run_pair, BULK_PEER, work, BULK_CALLS, 50)
        return {k: f.result() for k, f in futures.items()}


@pytest.mark.parametrize("case", CASES, ids=[c.peer.removesuffix(".xml") for c in CASES])
def test_every_exit_writes_one_valid_record(case: Case, pairings: dict[str, Pairing]) -> None:
    p = pairings[case.peer]
    assert p.uac_rc in (0, 1), f"{case.peer}: sipp exited {p.uac_rc}"
    if case.peer not in PEER_MAY_FAIL:
        assert p.peer_rc == 0, f"{case.peer}: the peer exited {p.peer_rc}"
    assert len(p.records) == 1, f"{case.peer}: {len(p.records)} records"
    r = p.records[0]
    assert SchemaSet.load().errors("call-outcome.v1", r) == []
    assert r["ending"] == case.ending
    assert r["final_code"] == case.final_code
    assert r["reason"] == case.reason
    assert r["phase_reached"] == case.phase
    assert r["early_media"] is case.early_media
    assert [f["kind"] for f in jv.objs(r, "failures")] == case.failures
    assert (r["first_provisional_at_ms"] is not None) is case.provisional
    assert r["run"] == "loopback" and r["stream"] == "basic" and r["shape"] == "basic"
    assert str(r["call_id"]).endswith("-loopback@127.0.0.1")


def test_a_response_landing_on_a_deadline_check_is_acted_on(pairings: dict[str, Pairing]) -> None:
    """The peer answers 1 s after its 180, where the caller checks its deadline:
    every call is answered, ACKed and torn down."""
    p = pairings["bulk"]
    assert p.peer_rc == 0
    assert len(p.records) == BULK_CALLS
    assert {str(r["ending"]) for r in p.records} == {"answered"}
    assert p.counter("TotalCallCreated") == BULK_CALLS
    # The handler did act: about four in ten of these answers land on the check.
    assert p.counter("GenericCounter2(C)") > 0
