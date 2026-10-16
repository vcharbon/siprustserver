"""The base UAS callee against SIPp peer callers on loopback.

Each pairing puts a request where one can land in the dialog (before the ACK,
mid-call, a CANCEL during the ring) or has the callee originate, and checks
both SIPp processes end as the pairing states: `ok` (both exit 0, every call
successful) or a callee failure on one stat column with the peer exiting 0.
The ring is 1 s, the callee's gap 500 ms. Needs `sipp` on PATH.
"""

from __future__ import annotations

import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

import pytest
from sipp_pairing import SIPP, UasRun, run_uas_pair


@dataclass(frozen=True)
class Case:
    peer: str
    expect: str
    calls: int = 1
    service: str = "service"

    @property
    def id(self) -> str:
        return f"{self.peer.removesuffix('.xml')}-{self.service}"


CASES = [
    Case("peer-uac-options-before-ack.xml", "ok"),
    Case("peer-uac-options-before-reinvite-ack.xml", "ok"),
    Case("peer-uac-options-then-bye.xml", "ok", calls=100),
    Case("peer-uac-requests-mid-call.xml", "ok"),
    Case("peer-uac-cancel-ring.xml", "ok"),
    Case("peer-uac-answers-callee-reinvites.xml", "ok", service="reinvite"),
    Case("peer-uac-no-ack-bye.xml", "FailedTestDoesntMatch"),
    Case("peer-uac-no-ack-options-bye.xml", "FailedTestDoesntMatch"),
    Case("peer-uac-reinvite-no-ack-bye.xml", "FailedTestDoesntMatch"),
]
KEYS = {"ring_ms": "1000", "gap_ms": "500"}


@pytest.fixture(scope="module")
def runs() -> dict[str, UasRun]:
    assert SIPP is not None, "sipp is not on PATH: the SIPp pairings are part of the gate"
    with tempfile.TemporaryDirectory(prefix="hakit-uas-") as tmp, ThreadPoolExecutor(len(CASES)) as pool:
        work = Path(tmp)
        futures = {c.id: pool.submit(run_uas_pair, c.peer, work, c.calls, 50, keys=KEYS, service=c.service) for c in CASES}
        return {k: f.result() for k, f in futures.items()}


@pytest.mark.parametrize("case", CASES, ids=[c.id for c in CASES])
def test_the_callee_serves_each_peer(case: Case, runs: dict[str, UasRun]) -> None:
    r = runs[case.id]
    assert r.peer_rc == 0, f"{case.peer}: the peer exited {r.peer_rc}"
    assert r.counter("TotalCallCreated") == case.calls
    if case.expect == "ok":
        assert r.sut_rc == 0, f"{case.peer}: the callee exited {r.sut_rc}"
        assert r.counter("SuccessfulCall(C)") == case.calls
    else:
        assert r.sut_rc == 1, f"{case.peer}: the callee exited {r.sut_rc}"
        assert r.counter(f"{case.expect}(C)") == case.calls
