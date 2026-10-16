"""Runs a base scenario against a SIPp peer on loopback and collects what it left."""

from __future__ import annotations

import csv
import json
import shutil
import socket
import subprocess
import tempfile
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from hakit.jsonval import Json, as_obj
from hakit.paths import BASE_CONTENT
from hakit.scenario_lint import key_defaults

PEERS = Path(__file__).parent / "sipp_peers"
SCENARIOS = BASE_CONTENT / "scenarios"
UAC = SCENARIOS / "uac-basic-hold.xml"
UAS = SCENARIOS / "uas-basic.xml"
SIPP = shutil.which("sipp")
# The keys every UAC pairing passes: a 3 s setup deadline, a 1.5 s hold.
UAC_KEYS = {"run": "loopback", "stream": "basic", "shape": "basic", "hold_ms": "1500", "setup_ms": "3000"}


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


def key_args(scenario: Path, keys: Mapping[str, str]) -> list[str]:
    """The scenario's declared defaults, then `keys` over them."""
    merged = {**key_defaults(scenario), **keys}
    return [a for k, v in sorted(merged.items()) for a in ("-key", k, v)]


def _wait(proc: subprocess.Popen[bytes], timeout_s: float) -> int | None:
    try:
        return proc.wait(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return None


def run_pair(
    peer: str | Path,
    work: Path,
    calls: int = 1,
    rate: int = 10,
    *,
    scenario: Path = UAC,
    keys: Mapping[str, str] | None = None,
    peer_keys: Mapping[str, str] | None = None,
    ruri_user: str = "service",
) -> Pairing:
    """`calls` calls of a UAC scenario at `rate` per second against a peer
    callee (a file of `sipp_peers`, or a scenario path)."""
    assert SIPP is not None, "sipp is not on PATH"
    peer_path = peer if isinstance(peer, Path) else PEERS / peer
    d = Path(tempfile.mkdtemp(prefix=f"{scenario.stem}-{peer_path.stem}-{calls}-", dir=work))
    inject = f"SEQUENTIAL\n{ruri_user};X-Test: loopback;;;\n"
    (d / "inject.csv").write_text(inject)
    caller = _free_port_pair()
    callee = caller + 1
    common = ["-i", "127.0.0.1", "-m", str(calls), "-nostdin", "-max_invite_retrans", "3", "-max_non_invite_retrans", "3"]
    proc = subprocess.Popen(
        [SIPP, "-sf", str(peer_path), "-p", str(callee), *common, *key_args(peer_path, peer_keys or {})]
        + ["-recv_timeout", "15000"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    uac = subprocess.run(
        [SIPP, f"127.0.0.1:{callee}", "-sf", str(scenario), "-p", str(caller), *common, "-r", str(rate)]
        + ["-inf", "inject.csv", *key_args(scenario, {**UAC_KEYS, **(keys or {})}), "-recv_timeout", "60000"]
        + ["-cid_str", "%u-%p-loopback@%s", "-trace_logs", "-log_file", "records.jsonl"]
        + ["-trace_err", "-trace_stat", "-stf", "stat.csv", "-fd", "1"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=180,
        check=False,
    )
    peer_rc = _wait(proc, 30)
    text = (d / "records.jsonl").read_text() if (d / "records.jsonl").is_file() else ""
    records = [as_obj(json.loads(line)) for line in text.splitlines() if line.strip()]
    return Pairing(records, uac.returncode, peer_rc, _last_stats(d / "stat.csv"))


@dataclass(frozen=True)
class UasRun:
    sut_rc: int | None
    peer_rc: int
    stats: dict[str, str]

    def counter(self, column: str) -> int:
        return int(self.stats.get(column, "0") or 0)


def run_uas_pair(
    peer: str,
    work: Path,
    calls: int = 1,
    rate: int = 10,
    *,
    keys: Mapping[str, str] | None = None,
    service: str = "service",
    scenario: Path = UAS,
) -> UasRun:
    """`calls` calls of a peer caller, R-URI user `service`, against a UAS scenario."""
    assert SIPP is not None, "sipp is not on PATH"
    d = Path(tempfile.mkdtemp(prefix=f"{scenario.stem}-{peer.removesuffix('.xml')}-{service}-{calls}-", dir=work))
    caller = _free_port_pair()
    callee = caller + 1
    common = ["-i", "127.0.0.1", "-m", str(calls), "-nostdin", "-max_invite_retrans", "3", "-max_non_invite_retrans", "3"]
    proc = subprocess.Popen(
        [SIPP, "-sf", str(scenario), "-p", str(callee), *common, *key_args(scenario, keys or {}), "-recv_timeout", "60000"]
        + ["-trace_err", "-trace_stat", "-stf", "stat.csv", "-fd", "1"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    peer_run = subprocess.run(
        [SIPP, f"127.0.0.1:{callee}", "-sf", str(PEERS / peer), "-p", str(caller), "-s", service, *common, "-r", str(rate)]
        + ["-recv_timeout", "30000", "-trace_err"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=180,
        check=False,
    )
    sut_rc = _wait(proc, 60)
    return UasRun(sut_rc, peer_run.returncode, _last_stats(d / "stat.csv"))
