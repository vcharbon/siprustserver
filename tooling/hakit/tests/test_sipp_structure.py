"""Structural rules of the base UAC scenario that SIPp's step engine imposes."""

from __future__ import annotations

import xml.etree.ElementTree as ET

from sipp_pairing import UAC

STEPS = ("nop", "send", "recv", "pause")


def test_a_resumed_call_lands_where_it_stood() -> None:
    """`_unexp.main` resumes with a jump to the step the call stood on, and SIPp
    then applies the `next` of the step before it. So a label reached by a jump
    follows no step with an unconditional `next`, but an unreachable `<nop/>`.
    A step with `test` and `next` is exempt: the call reached the following
    step because its test was false, and the resume evaluates it again, so it
    falls through again unless the handler changed the state the test reads
    (a provisional stamped while the call waits), and then it takes the branch
    that state calls for."""
    root = ET.parse(UAC).getroot()
    prev: ET.Element | None = None
    offenders: list[str] = []
    for e in root:
        if e.tag == "label":
            if prev is not None and prev.get("next") and not prev.get("test") and e.get("id") != "_unexp.main":
                offenders.append(str(e.get("id")))
        elif e.tag in STEPS:
            prev = e
    assert offenders == []


def test_every_optional_recv_names_its_next() -> None:
    """An optional recv without `next` leaves the call on the following recv,
    which may have no timeout of its own."""
    root = ET.parse(UAC).getroot()
    assert [e.attrib for e in root if e.tag == "recv" and e.get("optional") and not e.get("next")] == []


def test_the_invite_is_message_1() -> None:
    """The ACK of a non-2xx and the CANCEL take the INVITE's branch, z9hG4bK-[pid]-[call_number]-1."""
    steps = [e for e in ET.parse(UAC).getroot() if e.tag in STEPS]
    assert steps[1].tag == "send" and "INVITE sip:" in (steps[1].text or "")
