"""Runs the base UAC scenario against a SIPp peer on loopback and collects what it left."""

from __future__ import annotations

import csv
import json
import shutil
import socket
import subprocess
from dataclasses import dataclass
from pathlib import Path

from hakit.jsonval import Json, as_obj
from hakit.paths import BASE_CONTENT

PEERS = Path(__file__).parent / "sipp_peers"
UAC = BASE_CONTENT / "scenarios" / "uac-basic-hold.xml"
SIPP = shutil.which("sipp")


@dataclass(frozen=True)
class Pairing:
    records: list[Json]
    uac_rc: int
    peer_rc: int | None
    stats: dict[str, str]

    def counter(self, column: str) -> int:
        return int(self.stats.get(column, "0") or 0)


def _free_port_pair() -> int:
    for _ in range(200):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as a, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as b:
            a.bind(("127.0.0.1", 0))
            port = a.getsockname()[1]
            try:
                b.bind(("127.0.0.1", port + 1))
            except OSError:
                continue
            return port
    raise RuntimeError("no free UDP port pair")


def _last_stats(path: Path) -> dict[str, str]:
    if not path.is_file():
        return {}
    rows = list(csv.reader(path.read_text().splitlines(), delimiter=";"))
    return dict(zip(rows[0], rows[-1])) if len(rows) > 1 else {}


def run_pair(peer: str, work: Path, calls: int = 1, rate: int = 10) -> Pairing:
    """`calls` calls at `rate` per second: a 3 s setup deadline, a 1.5 s hold."""
    assert SIPP is not None, "sipp is not on PATH"
    d = work / f"{peer.removesuffix('.xml')}-{calls}"
    d.mkdir()
    (d / "inject.csv").write_text("SEQUENTIAL\nservice;X-Test: loopback;;;\n")
    caller = _free_port_pair()
    callee = caller + 1
    common = ["-i", "127.0.0.1", "-m", str(calls), "-nostdin", "-max_invite_retrans", "3", "-max_non_invite_retrans", "3"]
    proc = subprocess.Popen(
        [SIPP, "-sf", str(PEERS / peer), "-p", str(callee), *common, "-recv_timeout", "15000"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    uac = subprocess.run(
        [SIPP, f"127.0.0.1:{callee}", "-sf", str(UAC), "-p", str(caller), *common, "-r", str(rate)]
        + ["-inf", "inject.csv", "-key", "run", "loopback", "-key", "stream", "basic", "-key", "shape", "basic"]
        + ["-key", "hold_ms", "1500", "-key", "setup_ms", "3000", "-recv_timeout", "60000"]
        + ["-cid_str", "%u-%p-loopback@%s", "-trace_logs", "-log_file", "records.jsonl"]
        + ["-trace_stat", "-stf", "stat.csv", "-fd", "1"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=120,
        check=False,
    )
    try:
        peer_rc: int | None = proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        peer_rc = None
    text = (d / "records.jsonl").read_text() if (d / "records.jsonl").is_file() else ""
    records = [as_obj(json.loads(line)) for line in text.splitlines() if line.strip()]
    return Pairing(records, uac.returncode, peer_rc, _last_stats(d / "stat.csv"))
