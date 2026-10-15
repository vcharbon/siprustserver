"""The base UAC scenario writes one schema-valid call-outcome record at every exit.

Each pairing runs the scenario against a SIPp peer on loopback that drives one
exit (answer, reject, ring forever, silence, far-end BYE, 481 or no final to
the BYE, an in-dialog request the caller cannot serve) and checks the record
the call left. Needs `sipp` on PATH; skipped without it.
"""

from __future__ import annotations

import json
import shutil
import socket
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import pytest

from hakit import jsonval as jv
from hakit.jsonval import Json, as_obj
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

PEERS = Path(__file__).parent / "sipp_peers"
UAC = BASE_CONTENT / "scenarios" / "uac-basic-hold.xml"
SIPP = shutil.which("sipp")


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
    Case("peer-ring-no-answer.xml", "timeout", None, "timeout", "provisional"),
    Case("peer-silent.xml", "timeout", None, "timeout", "invited", provisional=False),
    Case("peer-bye-mid-hold.xml", "cut", 200, None, "released", provisional=False),
    Case("peer-bye-481.xml", "cut", 200, None, "released", failures=["dialog_request"]),
    Case("peer-bye-no-final.xml", "stranded", 200, None, "answered", failures=["dialog_request"]),
    Case("peer-info-mid-hold.xml", "failed", 200, None, "released", failures=["protocol"], provisional=False),
]


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


def _run_pair(case: Case, work: Path) -> list[Json]:
    d = work / case.peer.removesuffix(".xml")
    d.mkdir()
    (d / "inject.csv").write_text("SEQUENTIAL\nservice;X-Test: loopback;;;\n")
    caller = _free_port_pair()
    callee = caller + 1
    common = ["-i", "127.0.0.1", "-m", "1", "-nostdin", "-max_invite_retrans", "3", "-max_non_invite_retrans", "3"]
    peer = subprocess.Popen(
        [str(SIPP), "-sf", str(PEERS / case.peer), "-p", str(callee), *common, "-recv_timeout", "20000"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    uac = subprocess.run(
        [str(SIPP), f"127.0.0.1:{callee}", "-sf", str(UAC), "-p", str(caller), *common]
        + ["-inf", "inject.csv", "-key", "run", "loopback", "-key", "stream", "basic", "-key", "shape", "basic"]
        + ["-key", "hold_ms", "1500", "-key", "setup_ms", "3000", "-recv_timeout", "20000"]
        + ["-cid_str", "%u-%p-loopback@%s", "-trace_logs", "-log_file", "records.jsonl"],
        cwd=d,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=60,
        check=False,
    )
    try:
        peer.wait(timeout=30)
    except subprocess.TimeoutExpired:
        peer.kill()
    assert uac.returncode in (0, 1), f"{case.peer}: sipp exited {uac.returncode}"
    text = (d / "records.jsonl").read_text() if (d / "records.jsonl").is_file() else ""
    return [as_obj(json.loads(line)) for line in text.splitlines() if line.strip()]


@pytest.fixture(scope="module")
def records() -> dict[str, list[Json]]:
    if SIPP is None:
        pytest.skip("no sipp on PATH")
    with tempfile.TemporaryDirectory(prefix="hakit-sipp-") as tmp, ThreadPoolExecutor(len(CASES)) as pool:
        work = Path(tmp)
        futures = {c.peer: pool.submit(_run_pair, c, work) for c in CASES}
        return {k: f.result() for k, f in futures.items()}


@pytest.mark.parametrize("case", CASES, ids=[c.peer.removesuffix(".xml") for c in CASES])
def test_every_exit_writes_one_valid_record(case: Case, records: dict[str, list[Json]]) -> None:
    got = records[case.peer]
    assert len(got) == 1, f"{case.peer}: {len(got)} records"
    r = got[0]
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
