"""Runs a base incident's executable against fake `kubectl` and `docker`.

The fakes (`fakebin/fake_tool.py`) log every call and answer from scripted
rules, so a test states what the lab answers and checks what the executable
did, in order, without a cluster.
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path

from hakit.content import discover
from hakit.incident_exec import fmt_param
from hakit.jsonval import Json, as_obj
from hakit.paths import BASE_CONTENT, FAULT_LIB
from hakit.schemas import SchemaSet

FAKE_TOOL = Path(__file__).parent / "fakebin" / "fake_tool.py"
BASE = discover([BASE_CONTENT], SchemaSet.load())


@dataclass(frozen=True)
class Rule:
    match: str
    stdout: str = ""
    rc: int = 0
    once: bool = False


@dataclass(frozen=True)
class Call:
    argv: list[str]
    stdin: str

    @property
    def line(self) -> str:
        return " ".join(self.argv)


class Sandbox:
    def __init__(self, tmp: Path, incident: str, **params: str) -> None:
        self.inc = BASE.incidents[incident]
        self.bin = tmp / "bin"
        self.bin.mkdir(parents=True, exist_ok=True)
        for tool in ("kubectl", "docker"):
            link = self.bin / tool
            if not link.exists():
                link.symlink_to(FAKE_TOOL)
        self.event_dir = tmp / "event"
        self.event_dir.mkdir(exist_ok=True)
        self.log = tmp / "calls.jsonl"
        self.rules_file = tmp / "rules.json"
        self.rules_file.write_text("[]")
        target = self.inc.target
        self.env = {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "FAKE_LOG": str(self.log),
            "FAKE_RULES": str(self.rules_file),
            "HAKIT_RUN": "r-1",
            "HAKIT_RUN_DIR": str(tmp),
            "HAKIT_EVENT": "001-" + incident,
            "HAKIT_EVENT_DIR": str(self.event_dir),
            "HAKIT_INCIDENT": incident,
            "HAKIT_TARGET_SELECTOR": target.selector if target else "",
            "HAKIT_TARGET_NAMESPACE": "ns",
            "HAKIT_TARGET_PICK": target.pick if target else "one",
            "HAKIT_KUBE_CONTEXT": "kind-lab",
            "HAKIT_DURATION_S": "10",
            "HAKIT_LIB": str(FAULT_LIB),
            "HAKIT_VM_IMPORT": "http://127.0.0.1:1/api/v1/import/prometheus",
        }
        for name, p in self.inc.params.items():
            if p.default is not None:
                self.env[f"HAKIT_PARAM_{name.upper()}"] = fmt_param(p.default)
        for name, v in params.items():
            self.env[f"HAKIT_PARAM_{name.upper()}"] = v

    def rules(self, *rules: Rule) -> None:
        self.rules_file.write_text(json.dumps([asdict(r) for r in rules]))

    def state(self, name: str, text: str) -> None:
        (self.event_dir / name).write_text(text)

    def run(self, step: str) -> subprocess.CompletedProcess[str]:
        exe = self.inc.directory / step
        if self.log.exists():
            self.log.unlink()
        return subprocess.run([str(exe)], env=self.env, cwd=self.event_dir, capture_output=True, text=True, timeout=60)

    def calls(self) -> list[Call]:
        if not self.log.exists():
            return []
        out: list[Call] = []
        for line in self.log.read_text().splitlines():
            d = json.loads(line)
            out.append(Call(argv=[str(a) for a in d["argv"]], stdin=str(d["stdin"])))
        return out

    def index(self, needle: str) -> int:
        """The position of the first call whose line holds `needle`, -1 when none."""
        return next((i for i, c in enumerate(self.calls()) if needle in c.line), -1)


def facts(res: subprocess.CompletedProcess[str]) -> Json:
    assert res.returncode == 0, res.stderr
    return as_obj(json.loads(res.stdout))


def crash_rules(pod: str, node: str, containers: list[tuple[str, str, int]], alive: bool = False) -> list[Rule]:
    """The reads and kills of lib/crash.sh for one pod: its containers (name,
    id, host pid) at restart count 0, each process gone after the SIGKILL
    unless `alive`."""
    statuses = "".join(f"{name} containerd://{cid} 0\n" for name, cid, _ in containers)
    rules = [Rule(rf"get pod {pod} -o jsonpath={{range \.status\.containerStatuses\[\*\]}}{{\.name}}", statuses)]
    for _, cid, pid in containers:
        rules.append(Rule(rf"docker exec {node} crictl inspect --output go-template --template {{{{\.info\.pid}}}} {cid}$", str(pid)))
        rules.append(Rule(rf"docker exec {node} test -d /proc/{pid}$", rc=0 if alive else 1))
    return rules
