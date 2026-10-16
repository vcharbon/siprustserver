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
from sipp_pairing import SIPP, UasRun, run_uas_pair, sent_requests


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
    Case("peer-uac-invite-copy-after-200.xml", "ok"),
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
    assert r.peer_rc == 0, f"{case.peer}: the peer {r.peer_exit}"
    assert r.counter("TotalCallCreated") == case.calls
    if case.expect == "ok":
        assert r.sut_rc == 0, f"{case.peer}: the callee {r.sut_exit}"
        assert r.counter("SuccessfulCall(C)") == case.calls
    else:
        assert r.sut_rc == 1, f"{case.peer}: the callee {r.sut_exit}"
        assert r.counter(f"{case.expect}(C)") == case.calls


def test_every_request_of_the_callee_has_its_own_branch(tmp_path: Path) -> None:
    """RFC 3261 section 8.1.1.7: the callee's two re-INVITEs, their ACKs and its
    BYE each take a new branch, and its requests carry the dialog's local URI
    (the INVITE's To) in From (section 12.2.1.1)."""
    r = run_uas_pair("peer-uac-answers-callee-reinvites.xml", tmp_path, keys=KEYS, service="reinvite", trace=True)
    assert (r.peer_rc, r.sut_rc) == (0, 0), f"the peer {r.peer_exit}, the callee {r.sut_exit}"
    sent = sent_requests(r.messages)
    assert [m for m, _, _ in sent] == ["INVITE", "ACK", "INVITE", "ACK", "BYE"]
    assert len({b for _, _, b in sent}) == len(sent), sent
    froms = [ln for ln in r.messages.splitlines() if ln.startswith("From: <sip:reinvite@")]
    assert len(froms) >= 5, "the callee's requests do not carry its dialog URI in From"
