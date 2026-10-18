"""The run's packet capture: rings on the callers' bridge and inside the nodes
the profile names, kept per window at its close, stopped and handed back at
the end; `hakit pcap` extracts one call's packets by Call-ID."""

from __future__ import annotations

import os
import struct
from pathlib import Path

import pytest
from kit import budget, fault_incident, write_yaml

from hakit.capture import IMAGE, Captures
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.jsonval import Json
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.pcap import Packet, extract, read, write
from hakit.profile import Capture
from hakit.runner import Runner, RunRefused
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()
FUTURE_S = 1_900_000_000  # a ring file mtime later than any fake-clock window


def with_capture(tmp_path: Path) -> Content:
    root = tmp_path / "entry"
    fault_incident(root, "blip", duration_s=5)
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1", "name": "cap", "description": "d", "duration_s": 120, "drain_s": 130, "settle_s": 15,
        "setup_slow_ms": 1000, "catalogs": ["x-api-call"], "capture": {"file_mb": 10, "files": 4, "nodes": "tier=edge"},
        "streams": [{"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5,
                     "setup_deadline_s": 10, "ring_s": 5, "hold_s": 20}],
        "schedule": {"at": [{"at_s": 30, "incident": "blip", "budget": "any"}]},
    }
    write_yaml(root / "profiles" / "cap.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def lab_rules(c: Call) -> ProcResult | None:
    a = c.argv
    if c.kind != "run":
        return None
    if a[:3] == ["docker", "network", "inspect"]:
        return ProcResult(0, "<no value>\n" if "Options" in a[4] else "0123456789abcdef\n", "")
    if a[:1] == ["kubectl"] and "nodes" in a:
        return ProcResult(0, "e1 e2", "")
    if a[:2] == ["docker", "run"] and "tcpdump" in a:
        ring = Path(a[a.index("-v") + 1].split(":")[0])
        f = ring / "sip.pcap0"
        f.write_bytes(b"ring")
        os.utime(f, (FUTURE_S, FUTURE_S))
    return None


def test_a_run_captures_the_bridge_and_the_named_nodes_and_keeps_each_window(tmp_path: Path) -> None:
    clock, fake = simulated([lab_rules])
    r = Runner(with_capture(tmp_path), SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    run_dir = r.run("cap")
    runs = [c.argv for c in fake.calls if c.kind == "run" and c.argv[:2] == ["docker", "run"] and "tcpdump" in c.argv]
    assert len(runs) == 3
    bridge = runs[0]
    assert bridge[bridge.index("tcpdump"):] == ["tcpdump", "-i", "br-0123456789ab", "-s", "0", "-Z", "root", "-C", "10", "-W", "4",
                                               "-w", "/cap/sip.pcap", "udp and portrange 5060-5100"]
    assert "--net" in bridge and bridge[bridge.index("--net") + 1] == "host" and IMAGE in bridge
    nodes = [a[a.index("--net") + 1] for a in runs[1:]]
    assert nodes == ["container:e1", "container:e2"] and all(a[a.index("-i") + 1] == "any" for a in runs[1:])
    for source in ("bridge", "node-e1", "node-e2"):
        assert (run_dir / "pcap" / "windows" / "001-blip" / source / "sip.pcap0").read_bytes() == b"ring"
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    first_stream = next(i for i, a in enumerate(argvs) if a[:2] == ["docker", "run"] and "hakit-basic-exporter" in a)
    first_ring = next(i for i, a in enumerate(argvs) if a[:2] == ["docker", "run"] and "tcpdump" in a)
    assert first_ring < first_stream  # the capture holds the first call
    stops = [a for a in argvs if a[:2] == ["docker", "stop"]]
    chowns = [a for a in argvs if a[:2] == ["docker", "run"] and "chown" in a]
    assert len(stops) == 3 and len(chowns) == 3


def test_a_ring_that_does_not_start_refuses_the_run(tmp_path: Path) -> None:
    def refuse(c: Call) -> ProcResult | None:
        return ProcResult(125, "", "no such network") if c.kind == "run" and c.argv[:3] == ["docker", "network", "inspect"] else None

    clock, fake = simulated([refuse])
    r = Runner(with_capture(tmp_path), SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    with pytest.raises(RunRefused, match="network sipext: no such network"):
        r.run("cap")


def test_a_window_keeps_only_the_ring_files_written_since_it_opened(tmp_path: Path) -> None:
    caps = Captures("r", tmp_path, Capture(10, 4, "udp", None), lab_from_env({}), FakeExecutor())
    ring = tmp_path / "pcap" / "ring" / "bridge"
    ring.mkdir(parents=True)
    for name, mtime in (("sip.pcap0", 1_000), ("sip.pcap1", 1_996), ("sip.pcap2", 2_050)):
        (ring / name).write_bytes(name.encode())
        os.utime(ring / name, (mtime, mtime))
    from hakit.capture import Ring

    caps.rings.append(Ring("bridge", "c", ring))
    assert caps.keep("007-x", 2_000_000) == 2  # opened at 2000 s: 1996 is inside the 5 s margin
    assert sorted(p.name for p in (tmp_path / "pcap" / "windows" / "007-x" / "bridge").iterdir()) == ["sip.pcap1", "sip.pcap2"]


def udp(call_id: str, n: int) -> bytes:
    return b"\x00" * 42 + f"INVITE sip:x SIP/2.0\r\nCall-ID: {call_id}\r\nCSeq: {n} INVITE\r\n\r\n".encode()


def test_pcap_extracts_one_calls_packets_across_copies_and_link_types(tmp_path: Path) -> None:
    ring, window, node = (tmp_path / "pcap" / p for p in ("ring/bridge", "windows/001-x/bridge", "ring/node-e1"))
    for d in (ring, window, node):
        d.mkdir(parents=True)
    a1, a2, b1 = Packet(1_000, udp("call-a", 1)), Packet(3_000, udp("call-a", 2)), Packet(2_000, udp("call-b", 1))
    write(ring / "sip.pcap0", 1, [a1, b1, a2])
    write(window / "sip.pcap3", 1, [a1, b1])  # a copy of a ring file since rotated
    write(node / "sip.pcap0", 276, [Packet(1_500, b"\x00" * 20 + udp("call-a", 1))])
    (ring / "sip.pcap1").write_bytes(b"not a pcap")
    out = extract(tmp_path, "call-a", tmp_path / "calls")
    assert [p.name for p in out] == ["call-a.link1.pcap", "call-a.link276.pcap"]
    bridge = read(out[0])
    assert bridge.linktype == 1 and [p.ts_ns for p in bridge.packets] == [1_000, 3_000]
    assert read(out[1]).linktype == 276 and len(read(out[1]).packets) == 1


def test_pcap_matches_the_call_id_header_exactly(tmp_path: Path) -> None:
    ring = tmp_path / "pcap" / "ring" / "bridge"
    ring.mkdir(parents=True)
    compact = b"\x00" * 42 + b"BYE sip:x SIP/2.0\r\ni: 13-1-r@192.0.2.1\r\nCSeq: 2 BYE\r\n\r\n"
    longer = [Packet(n, udp(cid, 1)) for n, cid in ((2_000, "113-1-r@192.0.2.1"), (3_000, "13-1-r@192.0.2.10"))]
    write(ring / "sip.pcap0", 1, [Packet(1_000, udp("13-1-r@192.0.2.1", 1)), *longer, Packet(4_000, compact)])
    (out,) = extract(tmp_path, "13-1-r@192.0.2.1", tmp_path / "calls")
    assert [p.ts_ns for p in read(out).packets] == [1_000, 4_000]


def tcp(payload: bytes) -> bytes:
    """An Ethernet, IPv4 and TCP frame around `payload`; the TCP header's last byte is `x`."""
    eth = b"\x00" * 12 + b"\x08\x00"
    ip = bytes([0x45, 0]) + struct.pack(">H", 40 + len(payload)) + b"\x00" * 5 + b"\x06" + b"\x00" * 10
    return eth + ip + b"\x13\xc4\x13\xc4" + b"\x00" * 8 + b"\x50\x18\x00\x10" + b"\x00\x00\x00x" + payload


def test_pcap_reads_the_call_id_header_wherever_a_line_starts(tmp_path: Path) -> None:
    ring = tmp_path / "pcap" / "ring" / "bridge"
    ring.mkdir(parents=True)
    asked = "13-1-r@192.0.2.1"
    segment = tcp(f"Call-ID: {asked}\r\nCSeq: 2 INVITE\r\n\r\n".encode())  # the rest of a message cut by TCP
    lower = udp(asked, 3).replace(b"Call-ID:", b"call-id:")
    other = b"\x00" * 42 + f"INFO sip:x SIP/2.0\r\nCall-ID: 2-1-r@192.0.2.1\r\nX-Call-ID: {asked}\r\n\r\n".encode()
    write(ring / "sip.pcap0", 1, [Packet(1_000, segment), Packet(2_000, lower), Packet(3_000, other)])
    (out,) = extract(tmp_path, asked, tmp_path / "calls")
    assert [p.ts_ns for p in read(out).packets] == [1_000, 2_000]


def test_pcap_reads_a_microsecond_tcpdump_file_and_stops_at_a_torn_record(tmp_path: Path) -> None:
    head = struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 262144, 1)
    rec = struct.pack("<IIII", 5, 7, 4, 4) + b"abcd"
    (tmp_path / "f.pcap").write_bytes(head + rec + struct.pack("<IIII", 6, 0, 99, 99) + b"ab")
    pf = read(tmp_path / "f.pcap")
    assert [(p.ts_ns, p.data) for p in pf.packets] == [(5_000_007_000, b"abcd")]
