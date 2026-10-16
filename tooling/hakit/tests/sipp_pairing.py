"""Runs a base scenario against a SIPp peer on loopback and collects what it left.

A SIPp process killed by a signal (SIPp itself has crashed under load, the
peer with SIGSEGV) is reported as such, with the pid to look its core up by,
never as a scenario mismatch.
"""

from __future__ import annotations

import csv
import json
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
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


def exit_note(rc: int | None, pid: int) -> str:
    """How a SIPp process ended, in words a failed assertion can carry."""
    if rc is None:
        return f"did not exit in time (pid {pid}, killed)"
    if rc < 0:
        name = signal.Signals(-rc).name
        return f"was killed by {name} (pid {pid}; core: `coredumpctl info {pid}`, journal: `journalctl _PID={pid}`)"
    if rc == 254:
        return "could not bind its port (exit 254)"
    return f"exited {rc}"


@dataclass(frozen=True)
class Pairing:
    records: list[Json]
    uac_rc: int
    peer_rc: int | None
    stats: dict[str, str]
    uac_pid: int = 0
    peer_pid: int = 0
    messages: str = ""

    def counter(self, column: str) -> int:
        return int(self.stats.get(column, "0") or 0)

    @property
    def uac_exit(self) -> str:
        return exit_note(self.uac_rc, self.uac_pid)

    @property
    def peer_exit(self) -> str:
        return exit_note(self.peer_rc, self.peer_pid)


_TAKEN: set[int] = set()
_TAKEN_LOCK = threading.Lock()


def _free_port_pair() -> int:
    """A pair of adjacent free UDP ports no other pairing of this process got:
    the pairings run in parallel, and a port probed free is bound by SIPp
    only later."""
    with _TAKEN_LOCK:
        port = _probe_pair()
        _TAKEN.update((port, port + 1))
        return port


def _probe_pair() -> int:
    for _ in range(200):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as a, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as b:
            a.bind(("127.0.0.1", 0))
            port = a.getsockname()[1]
            if port in _TAKEN or port + 1 in _TAKEN:
                continue
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


def _read(p: Path) -> str:
    return p.read_text(errors="replace") if p.is_file() else ""


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
    trace: bool = False,
) -> Pairing:
    """`calls` calls of a UAC scenario at `rate` per second against a peer
    callee (a file of `sipp_peers`, or a scenario path); `trace` keeps the
    UAC's message log."""
    assert SIPP is not None, "sipp is not on PATH"
    peer_path = peer if isinstance(peer, Path) else PEERS / peer
    d = Path(tempfile.mkdtemp(prefix=f"{scenario.stem}-{peer_path.stem}-{calls}-", dir=work))
    (d / "inject.csv").write_text(f"SEQUENTIAL\n{ruri_user};X-Test: loopback;;;\n")
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
    traces = ["-trace_msg", "-message_file", "messages.log"] if trace else []
    uac = subprocess.Popen(
        [SIPP, f"127.0.0.1:{callee}", "-sf", str(scenario), "-p", str(caller), *common, "-r", str(rate)]
        + ["-inf", "inject.csv", *key_args(scenario, {**UAC_KEYS, **(keys or {})}), "-recv_timeout", "60000"]
        + ["-cid_str", "%u-%p-loopback@%s", "-trace_logs", "-log_file", "records.jsonl"]
        + ["-trace_err", "-trace_stat", "-stf", "stat.csv", "-fd", "1", *traces],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    uac_rc = _wait(uac, 180)
    peer_rc = _wait(proc, 30)
    records = [as_obj(json.loads(line)) for line in _read(d / "records.jsonl").splitlines() if line.strip()]
    return Pairing(
        records,
        uac_rc if uac_rc is not None else -9,
        peer_rc,
        _last_stats(d / "stat.csv"),
        uac.pid,
        proc.pid,
        _read(d / "messages.log"),
    )


@dataclass(frozen=True)
class UasRun:
    sut_rc: int | None
    peer_rc: int
    stats: dict[str, str]
    sut_pid: int = 0
    peer_pid: int = 0
    messages: str = ""

    def counter(self, column: str) -> int:
        return int(self.stats.get(column, "0") or 0)

    @property
    def sut_exit(self) -> str:
        return exit_note(self.sut_rc, self.sut_pid)

    @property
    def peer_exit(self) -> str:
        return exit_note(self.peer_rc, self.peer_pid)


def run_uas_pair(
    peer: str,
    work: Path,
    calls: int = 1,
    rate: int = 10,
    *,
    keys: Mapping[str, str] | None = None,
    service: str = "service",
    scenario: Path = UAS,
    trace: bool = False,
) -> UasRun:
    """`calls` calls of a peer caller, R-URI user `service`, against a UAS
    scenario; `trace` keeps the UAS's message log."""
    assert SIPP is not None, "sipp is not on PATH"
    d = Path(tempfile.mkdtemp(prefix=f"{scenario.stem}-{peer.removesuffix('.xml')}-{service}-{calls}-", dir=work))
    caller = _free_port_pair()
    callee = caller + 1
    common = ["-i", "127.0.0.1", "-m", str(calls), "-nostdin", "-max_invite_retrans", "3", "-max_non_invite_retrans", "3"]
    traces = ["-trace_msg", "-message_file", "messages.log"] if trace else []
    proc = subprocess.Popen(
        [SIPP, "-sf", str(scenario), "-p", str(callee), *common, *key_args(scenario, keys or {}), "-recv_timeout", "60000"]
        + ["-trace_err", "-trace_stat", "-stf", "stat.csv", "-fd", "1", *traces],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    peer_proc = subprocess.Popen(
        [SIPP, f"127.0.0.1:{callee}", "-sf", str(PEERS / peer), "-p", str(caller), "-s", service, *common, "-r", str(rate)]
        + ["-recv_timeout", "30000", "-trace_err"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    peer_rc = _wait(peer_proc, 180)
    sut_rc = _wait(proc, 60)
    return UasRun(
        sut_rc,
        peer_rc if peer_rc is not None else -9,
        _last_stats(d / "stat.csv"),
        proc.pid,
        peer_proc.pid,
        _read(d / "messages.log"),
    )


def sent_requests(messages: str) -> list[tuple[str, str, str]]:
    """(method, CSeq, top Via branch) of every request a SIPp message log shows sent."""
    out: list[tuple[str, str, str]] = []
    for block in messages.split("UDP message sent")[1:]:
        lines = [ln.strip() for ln in block.split("\n")[2:] if ln.strip()]
        if not lines or lines[0].startswith("SIP/2.0"):
            continue
        method = lines[0].split(" ", 1)[0]
        via = next((ln for ln in lines if ln.lower().startswith("via:")), "")
        cseq = next((ln for ln in lines if ln.lower().startswith("cseq:")), "")
        branch = via.split("branch=", 1)[1].split(";", 1)[0] if "branch=" in via else ""
        out.append((method, cseq.split(":", 1)[1].strip() if cseq else "", branch))
    return out
