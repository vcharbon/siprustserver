"""`hakit pcap <run-dir> <call-id>`: one call's packets out of a run's capture.

Reads every classic pcap file under the run directory's `pcap/` (the window
copies and the last ring), keeps the packets whose Call-ID header (`Call-ID`
or its compact `i`) holds the Call-ID asked, byte for byte (RFC 3261 §20.8),
on a line of the UDP or TCP payload (a TCP segment may begin with it), drops the copies a window and the ring share, and writes them in time order,
one file per link type (the bridge ring and a node's `any` ring differ).
"""

from __future__ import annotations

import re
import struct
from dataclasses import dataclass
from pathlib import Path

MAGICS = {
    b"\xd4\xc3\xb2\xa1": ("<", 1_000),
    b"\xa1\xb2\xc3\xd4": (">", 1_000),
    b"\x4d\x3c\xb2\xa1": ("<", 1),
    b"\xa1\xb2\x3c\x4d": (">", 1),
}


@dataclass(frozen=True)
class Packet:
    ts_ns: int
    data: bytes


@dataclass(frozen=True)
class PcapFile:
    linktype: int
    snaplen: int
    packets: list[Packet]


class PcapError(ValueError):
    pass


def read(path: Path) -> PcapFile:
    raw = path.read_bytes()
    if len(raw) < 24 or raw[:4] not in MAGICS:
        raise PcapError(f"{path}: not a classic pcap file")
    order, ns_per_frac = MAGICS[raw[:4]]
    _, _, _, _, snaplen, linktype = struct.unpack(order + "HHiIII", raw[4:24])
    packets: list[Packet] = []
    off = 24
    while off + 16 <= len(raw):
        sec, frac, incl, _orig = struct.unpack(order + "IIII", raw[off : off + 16])
        off += 16
        if off + incl > len(raw):
            break  # a file still being written ends mid-record
        packets.append(Packet(sec * 1_000_000_000 + frac * ns_per_frac, raw[off : off + incl]))
        off += incl
    return PcapFile(linktype, snaplen, packets)


def write(path: Path, linktype: int, packets: list[Packet]) -> None:
    out = bytearray(struct.pack("<IHHiIII", 0xA1B23C4D, 2, 4, 0, 0, 262_144, linktype))
    for p in packets:
        sec, ns = divmod(p.ts_ns, 1_000_000_000)
        out += struct.pack("<IIII", sec, ns, len(p.data), len(p.data)) + p.data
    path.write_bytes(bytes(out))


# The link header ahead of the network layer, and where its EtherType sits.
LINKS = {1: (14, 12), 113: (16, 14), 276: (20, 0)}
VLAN = 0x8100
IPV4, IPV6 = 0x0800, 0x86DD
TCP, UDP = 6, 17


def payload(linktype: int, data: bytes) -> bytes:
    """The UDP or TCP payload of a frame; the whole frame when it is no
    unfragmented IPv4 or IPv6 UDP or TCP packet of a known link type."""
    if linktype not in LINKS:
        return data
    off, at = LINKS[linktype]
    if len(data) < off:
        return data
    ether = int.from_bytes(data[at : at + 2])
    while linktype == 1 and ether == VLAN and len(data) >= off + 4:
        ether, off = int.from_bytes(data[off + 2 : off + 4]), off + 4
    if ether == IPV4 and len(data) >= off + 20:
        proto, ip_len = data[off + 9], (data[off] & 0x0F) * 4
        if int.from_bytes(data[off + 6 : off + 8]) & 0x1FFF:
            return data  # a later fragment carries no transport header
    elif ether == IPV6 and len(data) >= off + 40:
        proto, ip_len = data[off + 6], 40
    else:
        return data
    off += ip_len
    if proto == UDP and len(data) >= off + 8:
        return data[off + 8 :]
    if proto == TCP and len(data) >= off + 20:
        return data[off + (data[off + 12] >> 4) * 4 :]
    return data


def call_id_header(call_id: str) -> re.Pattern[bytes]:
    """A Call-ID header line whose whole value is `call_id`; the header name is case-insensitive."""
    return re.compile(rb"(?m)^(?i:call-id|i)[ \t]*:[ \t]*" + re.escape(call_id.encode()) + rb"[ \t]*\r?$")


def extract(run_dir: Path, call_id: str, out_dir: Path) -> list[Path]:
    """Write the call's packets under `out_dir`; the files written."""
    header = call_id_header(call_id)
    by_link: dict[int, dict[tuple[int, bytes], Packet]] = {}
    for f in sorted((run_dir / "pcap").rglob("sip.pcap*")):
        try:
            pf = read(f)
        except PcapError:
            continue
        kept = by_link.setdefault(pf.linktype, {})
        for p in pf.packets:
            if header.search(payload(pf.linktype, p.data)):
                kept[(p.ts_ns, p.data)] = p
    out_dir.mkdir(parents=True, exist_ok=True)
    stem = re.sub(r"[^A-Za-z0-9._-]", "_", call_id)
    written: list[Path] = []
    for linktype, kept in sorted(by_link.items()):
        if not kept:
            continue
        dest = out_dir / f"{stem}.link{linktype}.pcap"
        write(dest, linktype, sorted(kept.values(), key=lambda p: p.ts_ns))
        written.append(dest)
    return written
