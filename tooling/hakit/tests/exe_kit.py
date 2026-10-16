"""Runs a base incident's executable against fake `kubectl` and `docker`.

The fakes (`fakebin/fake_tool.py`) log every call and answer from scripted
rules; a call no rule scripts exits 97. So a test states every answer the lab
gives and checks what the executable did, in order, without a cluster. The
rule builders below script the reads a library function makes.
"""

from __future__ import annotations

import json
import os
import re
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
UNSCRIPTED = 97


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
    at_ms: int = 0

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
            "HK_RECHECK_S": "0",
            "HK_CRASH_WAIT_S": "1",
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
            out.append(Call(argv=[str(a) for a in d["argv"]], stdin=str(d["stdin"]), at_ms=int(d.get("at_ms", 0))))
        return out

    def index(self, needle: str) -> int:
        """The position of the first call whose line holds `needle`, -1 when none."""
        return next((i for i, c in enumerate(self.calls()) if needle in c.line), -1)

    def lines(self, needle: str) -> list[str]:
        return [c.line for c in self.calls() if needle in c.line]


def facts(res: subprocess.CompletedProcess[str]) -> Json:
    assert res.returncode == 0, res.stderr
    assert "unscripted call" not in res.stderr, res.stderr
    return as_obj(json.loads(res.stdout))


def failed(res: subprocess.CompletedProcess[str], why: str) -> None:
    """The step failed for `why`, not because a call went unscripted."""
    assert res.returncode not in (0, 75, UNSCRIPTED), (res.returncode, res.stderr)
    assert "unscripted call" not in res.stderr, res.stderr
    assert why in res.stderr, res.stderr


def live(selector: str, rows: str) -> Rule:
    """`hk_live_pods` of a selector: "name uid node True" rows."""
    return Rule(rf"get pods -l {re.escape(selector)} -o jsonpath=\{{range .items\[\*\]\}}\{{\.metadata\.name\}}", rows)


def pod_rules(pod: str, node: str, uid: str, *, frozen: str | list[str] = "0", qdisc: str | list[str] = "qdisc noqueue 0: root",
              iptables: str | list[str] = "-P INPUT ACCEPT\n", restarts: str = "0\n", host_network: str = "") -> list[Rule]:
    """The reads pod-faults.sh makes for one pod on its node: its node, uid,
    sandbox netns and pod-level cgroup, then the residue reads; a list of
    answers is given once each in order, the last one for good."""

    def seq(match: str, answers: str | list[str]) -> list[Rule]:
        values = [answers] if isinstance(answers, str) else answers
        return [Rule(match, v, once=True) for v in values[:-1]] + [Rule(match, values[-1])]

    p = re.escape(pod)
    return [
        Rule(rf"get pod {p} --ignore-not-found -o jsonpath=\{{\.metadata\.uid\}}$", uid),
        Rule(rf"get pod {p} --ignore-not-found -o jsonpath=\{{range \.status\.containerStatuses", restarts),
        Rule(rf"get pod {p} -o jsonpath=\{{\.spec\.nodeName\}}$", node),
        Rule(rf"get pod {p} -o jsonpath=\{{\.metadata\.uid\}}$", uid),
        Rule(rf"get pod {p} -o jsonpath=\{{\.spec\.hostNetwork\}}$", host_network),
        Rule(rf"docker exec {node} crictl pods --label io.kubernetes.pod.uid={uid} --state ready -q$", "sb1\n"),
        Rule(rf"docker exec {node} crictl inspectp --output go-template --template \{{\{{\.info\.pid\}}\}} sb1$", "111"),
        Rule(rf"docker exec {node} cat /proc/111/cgroup$", "0::/kubepods/pod1/sb1\n"),
        *seq(rf"docker exec {node} cat /sys/fs/cgroup/kubepods/pod1/cgroup\.freeze$", frozen),
        Rule(rf"docker exec {node} sh -c echo [01] > '/sys/fs/cgroup/kubepods/pod1/cgroup\.freeze'$"),
        *seq(rf"docker exec {node} nsenter -t 111 -n -- tc qdisc show dev eth0$", qdisc),
        *seq(rf"docker exec {node} nsenter -t 111 -n -- iptables -S INPUT$", iptables),
    ]


def crash_rules(pod: str, node: str, containers: list[tuple[str, str]], *, after: str = "CONTAINER_EXITED",
                restarts: int = 0) -> list[Rule]:
    """The reads and kills of lib/crash.sh for one pod: its containers (name,
    id) running at `restarts`, each SIGKILLed by id, then in state `after`."""
    statuses = "".join(f"{name} containerd://{cid} {restarts}\n" for name, cid in containers)
    rules = [Rule(rf"get pod {re.escape(pod)} -o jsonpath=\{{range \.status\.containerStatuses\[\*\]\}}\{{\.name\}}", statuses)]
    for _, cid in containers:
        state = rf"docker exec {node} crictl inspect --output go-template --template \{{\{{\.status\.state\}}\}} {cid}$"
        rules.append(Rule(state, "CONTAINER_RUNNING", once=True))
        rules.append(Rule(rf"docker exec {node} ctr -n k8s.io task kill -s SIGKILL {cid}$"))
        rules.append(Rule(state, after))
    return rules
