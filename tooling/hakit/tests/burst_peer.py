"""A callee on one UDP socket that puts two messages on the wire in one burst.

SIPp sends one message of a call per scheduler step, so a SIPp peer cannot
queue two datagrams of one call before the caller reads the first. This peer answers each
INVITE with a 180 Ringing, then sends an OPTIONS in the early dialog and the
200 OK in two back-to-back `sendto` calls: the 200 lands while the caller
still serves the OPTIONS. The 200 retransmits on T1 doubling to T2 until the
ACK (RFC 3261 §13.3.1.4); any later request of the caller is answered 200.
The OPTIONS is sent once (loopback loses nothing). A call is complete once
the OPTIONS got its 200, the 200 its ACK, and the caller's BYE its 200.
"""

from __future__ import annotations

import re
import socket
import time
from dataclasses import dataclass

T1, T2 = 0.5, 4.0


@dataclass
class _Call:
    caller: tuple[str, int]
    ok: str
    ok_next: float
    ok_delay: float = T1
    acked: bool = False
    options_done: bool = False
    bye_done: bool = False

    @property
    def complete(self) -> bool:
        return self.acked and self.options_done and self.bye_done


def _parse(data: bytes) -> tuple[str, dict[str, str]]:
    head = data.decode(errors="replace").split("\r\n\r\n", 1)[0].split("\r\n")
    headers: dict[str, str] = {}
    for line in head[1:]:
        name, _, value = line.partition(":")
        headers.setdefault(name.strip().lower(), value.strip())
    return head[0], headers


def _uri(header: str) -> str:
    m = re.search(r"<([^>]+)>", header)
    return m.group(1) if m else header.split(";")[0].strip()


class BurstPeer:
    """Serves `calls` calls on the bound UDP `sock`."""

    def __init__(self, sock: socket.socket, calls: int) -> None:
        self.sock = sock
        self.calls = calls
        self.host, self.port = sock.getsockname()[:2]
        self.by_id: dict[str, _Call] = {}

    def _send(self, text: str, to: tuple[str, int]) -> None:
        self.sock.sendto(text.encode(), to)

    def _response(self, status: str, h: dict[str, str], to_tag: str | None, body: str = "") -> str:
        to = h["to"] + (f";tag={to_tag}" if to_tag else "")
        sdp = "Content-Type: application/sdp\r\n" if body else ""
        return (
            f"SIP/2.0 {status}\r\nVia: {h['via']}\r\nFrom: {h['from']}\r\nTo: {to}\r\n"
            f"Call-ID: {h['call-id']}\r\nCSeq: {h['cseq']}\r\n"
            f"Contact: <sip:{self.host}:{self.port};transport=udp>\r\n"
            f"{sdp}Content-Length: {len(body)}\r\n\r\n{body}"
        )

    def _options(self, h: dict[str, str], tag: str) -> str:
        return (
            f"OPTIONS {_uri(h['contact'])} SIP/2.0\r\n"
            f"Via: SIP/2.0/UDP {self.host}:{self.port};branch=z9hG4bK-options-{tag}\r\n"
            f"From: {h['to']};tag={tag}\r\nTo: {h['from']}\r\nCall-ID: {h['call-id']}\r\n"
            "CSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )

    def _on_invite(self, h: dict[str, str], src: tuple[str, int]) -> None:
        c = self.by_id.get(h["call-id"])
        if c is not None:
            if not c.acked:
                self._send(c.ok, src)
            return
        tag = f"burst{len(self.by_id) + 1}"
        sdp = (
            f"v=0\r\no=callee 1 1 IN IP4 {self.host}\r\ns=-\r\nc=IN IP4 {self.host}\r\n"
            "t=0 0\r\nm=audio 6000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
        )
        c = _Call(src, self._response("200 OK", h, tag, sdp), time.monotonic() + T1)
        self.by_id[h["call-id"]] = c
        self._send(self._response("180 Ringing", h, tag), src)
        self._send(self._options(h, tag), src)
        self._send(c.ok, src)

    def _on_request(self, method: str, h: dict[str, str], src: tuple[str, int]) -> None:
        c = self.by_id.get(h.get("call-id", ""))
        if c is None:
            return
        if method == "ACK":
            c.acked = True
            return
        self._send(self._response("200 OK", h, None), src)
        if method == "BYE":
            c.bye_done = True

    def _on_response(self, start: str, h: dict[str, str]) -> None:
        c = self.by_id.get(h.get("call-id", ""))
        if c is not None and start.startswith("SIP/2.0 200") and h.get("cseq", "").endswith("OPTIONS"):
            c.options_done = True

    def _retransmit(self, now: float) -> None:
        for c in self.by_id.values():
            if not c.acked and now >= c.ok_next:
                self._send(c.ok, c.caller)
                c.ok_delay = min(c.ok_delay * 2, T2)
                c.ok_next = now + c.ok_delay

    def serve(self, deadline_s: float) -> int:
        """Serves until every call is complete or `deadline_s` passed; the
        number of calls not complete."""
        end = time.monotonic() + deadline_s
        self.sock.settimeout(0.05)
        while time.monotonic() < end:
            if len(self.by_id) == self.calls and all(c.complete for c in self.by_id.values()):
                break
            try:
                data, src = self.sock.recvfrom(65535)
            except TimeoutError:
                data, src = b"", ("", 0)
            if data.strip():
                start, h = _parse(data)
                if start.startswith("SIP/2.0"):
                    self._on_response(start, h)
                elif start.startswith("INVITE "):
                    self._on_invite(h, src)
                else:
                    self._on_request(start.split(" ", 1)[0], h, src)
            self._retransmit(time.monotonic())
        return self.calls - sum(c.complete for c in self.by_id.values())
