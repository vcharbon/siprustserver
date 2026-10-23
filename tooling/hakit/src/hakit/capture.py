"""The run's packet capture (a profile's `capture`): rotating tcpdump rings,
kept per window.

One ring on the callers' docker bridge, and one inside each kind node the
profile's `nodes` selector names (every interface of the node's netns). Each
ring is a container of the capture image (`tooling/hakit/capture/`, built on
first use) writing `pcap/ring/<source>/sip.pcapN` in the run directory. When a
window closes, the ring files written since it opened (by mtime, with a margin)
are copied to `pcap/windows/<event>/<source>/`, so what an incident caused
outlives the ring's rotation. At the end of the run the rings are stopped,
their files handed to the runner's user, and the last ring kept as it stands.
"""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path

from .executor import Executor
from .lab import Lab
from .paths import HAKIT_ROOT
from .profile import Capture

IMAGE = "hakit-capture:1"
IMAGE_DIR = HAKIT_ROOT / "capture"
# Ring files written this long before a window opened are kept with it.
KEEP_MARGIN_MS = 5_000


@dataclass(frozen=True)
class Ring:
    source: str
    container: str
    directory: Path


class CaptureFailed(Exception):
    pass


class Captures:
    def __init__(self, run: str, run_dir: Path, spec: Capture, lab: Lab, executor: Executor) -> None:
        self.run = run
        self.run_dir = run_dir
        self.spec = spec
        self.lab = lab
        self.executor = executor
        self.rings: list[Ring] = []

    def _run(self, argv: list[str], what: str, timeout_s: float = 60) -> str:
        res = self.executor.run(argv, timeout_s=timeout_s)
        if res.rc != 0:
            raise CaptureFailed(f"{what}: {res.stderr.strip()[:300]}")
        return res.stdout.strip()

    def _bridge(self) -> str:
        """The host interface of the callers' docker network."""
        net = self.lab.sipp_network
        name = self._run(["docker", "network", "inspect", "-f", '{{index .Options "com.docker.network.bridge.name"}}', net], f"network {net}")
        if name and name != "<no value>":
            return name
        return "br-" + self._run(["docker", "network", "inspect", "-f", "{{.Id}}", net], f"network {net}")[:12]

    def _nodes(self) -> list[str]:
        if not self.spec.nodes:
            return []
        out = self._run(
            ["kubectl", "--context", self.lab.kube_context, "get", "nodes", "-l", self.spec.nodes, "-o", "jsonpath={.items[*].metadata.name}"],
            f"nodes {self.spec.nodes}",
        )
        return out.split()

    def _image(self) -> None:
        if self.executor.run(["docker", "image", "inspect", IMAGE], timeout_s=30).rc != 0:
            self._run(["docker", "build", "-t", IMAGE, str(IMAGE_DIR)], f"build {IMAGE}", timeout_s=600)

    def start(self) -> None:
        """Start every ring; a ring that does not start refuses the run."""
        self._image()
        targets = [("bridge", ["--net", "host"], self._bridge())]
        targets += [(f"node-{n}", ["--net", f"container:{n}"], "any") for n in self._nodes()]
        for source, net, iface in targets:
            d = self.run_dir / "pcap" / "ring" / source
            d.mkdir(parents=True)
            name = f"hakit-capture-{self.run}-{source}"[:128]
            self._run(
                ["docker", "run", "-d", "--name", name, "--label", f"hakit-run={self.run}", *net]
                + ["--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW", "-v", f"{d}:/cap", IMAGE]
                + ["tcpdump", "-i", iface, "-s", "0", "-Z", "root", "-C", str(self.spec.file_mb), "-W", str(self.spec.files)]
                + ["-w", "/cap/sip.pcap", self.spec.filter],
                f"capture {source}",
            )
            self.rings.append(Ring(source, name, d))

    def keep(self, event: str, opened_ms: int) -> int:
        """Copy every ring file written since the window opened; the count."""
        n = 0
        since = (opened_ms - KEEP_MARGIN_MS) / 1000
        for ring in self.rings:
            dest = self.run_dir / "pcap" / "windows" / event / ring.source
            for f in sorted(ring.directory.glob("sip.pcap*")):
                if f.stat().st_mtime >= since:
                    dest.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(f, dest / f.name)
                    n += 1
        return n

    def stop(self) -> None:
        """Stop every ring (tcpdump flushes) and hand its files to the runner's user."""
        for ring in self.rings:
            self.executor.run(["docker", "stop", "-t", "5", ring.container], timeout_s=60)
            self.executor.run(
                ["docker", "run", "--rm", "-v", f"{ring.directory}:/cap", IMAGE, "chown", "-R", f"{os.getuid()}:{os.getgid()}", "/cap"],
                timeout_s=60,
            )
            self.executor.run(["docker", "rm", "-f", ring.container], timeout_s=60)
