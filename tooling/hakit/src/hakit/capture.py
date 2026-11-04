"""The run's packet capture (a profile's `capture`): rotating tcpdump rings,
kept per window.

One ring on the callers' docker bridge, and one inside each kind node the
profile's `nodes` selector names (every interface of the node's netns). Each
ring is a container of the capture image (`tooling/hakit/capture/`, built on
first use) writing `pcap/ring/<source>/sip.pcapN` in the run directory. A stale
ring (its container stopped, or its node started after it: a node ring keeps
running in the namespace of a node that stopped) is started again at
each window's close and before the run's end, each new incarnation n writing
`sip-<n>.pcapN` beside the earlier files. When a window closes, the ring files
written since it opened (by mtime, with a margin) are copied to
`pcap/windows/<event>/<source>/`, so what an incident caused outlives the
ring's rotation. At the end of the run the rings are stopped, their files
handed to the runner's user, and the last ring kept as it stands.
"""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from .executor import Executor, ProcResult
from .lab import Lab
from .paths import HAKIT_ROOT
from .profile import Capture

IMAGE = "hakit-capture:1"
IMAGE_DIR = HAKIT_ROOT / "capture"
# Ring files written this long before a window opened are kept with it.
KEEP_MARGIN_MS = 5_000
# Every incarnation's ring files: `sip.pcapN`, then `sip-<n>.pcapN`.
RING_GLOB = "sip*.pcap*"


@dataclass
class Ring:
    source: str
    container: str
    directory: Path
    net: list[str]
    iface: str
    bpf: str
    # 0 for the ring the run started; each revival adds one.
    incarnation: int = 0

    def file(self) -> str:
        return "/cap/sip.pcap" if self.incarnation == 0 else f"/cap/sip-{self.incarnation}.pcap"

    def node(self) -> str | None:
        """The kind node whose network namespace the ring joined; None for the bridge."""
        joined = next((n for n in self.net if n.startswith("container:")), None)
        return joined.removeprefix("container:") if joined else None


def started_ns(stamp: str) -> int | None:
    """An RFC 3339 `State.StartedAt` in nanoseconds since the epoch (UTC, any
    fraction length); None when it is not one."""
    stamp = stamp.strip()
    if not stamp.endswith("Z") or "T" not in stamp:
        return None
    whole, _, frac = stamp[:-1].partition(".")
    try:
        seconds = int(datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc).timestamp())
        nanos = int((frac + "000000000")[:9]) if frac else 0
    except ValueError:
        return None
    return seconds * 1_000_000_000 + nanos


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

    def _bridge_filter(self, bridge: str) -> str:
        """The profile's filter without the packets the host itself sends on the
        bridge: a caller's request to an address the host routes (a VIP) crosses
        the bridge twice, from the caller and again from the host to the next
        hop; the ring keeps what the callers sent, and the node rings show what
        each next hop received."""
        mac = (self.executor.read_text(Path(f"/sys/class/net/{bridge}/address")) or "").strip()
        if not mac:
            return self.spec.filter
        return f"({self.spec.filter}) and not ether src {mac}"

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
        bridge = self._bridge()
        targets = [("bridge", ["--net", "host"], bridge, self._bridge_filter(bridge))]
        targets += [(f"node-{n}", ["--net", f"container:{n}"], "any", self.spec.filter) for n in self._nodes()]
        for source, net, iface, bpf in targets:
            d = self.run_dir / "pcap" / "ring" / source
            d.mkdir(parents=True)
            ring = Ring(source, f"hakit-capture-{self.run}-{source}"[:128], d, net, iface, bpf)
            res = self._launch(ring)
            if res.rc != 0:
                raise CaptureFailed(f"capture {source}: {res.stderr.strip()[:300]}")
            self.rings.append(ring)

    def _launch(self, ring: Ring) -> ProcResult:
        return self.executor.run(
            ["docker", "run", "-d", "--name", ring.container, "--label", f"hakit-run={self.run}", *ring.net]
            + ["--cap-add", "NET_ADMIN", "--cap-add", "NET_RAW", "-v", f"{ring.directory}:/cap", IMAGE]
            + ["tcpdump", "-i", ring.iface, "-s", "0", "-Z", "root", "-C", str(self.spec.file_mb), "-W", str(self.spec.files)]
            + ["-w", ring.file(), ring.bpf],
            timeout_s=60,
        )

    def _stale(self, ring: Ring) -> bool:
        """Whether the ring captures nothing any more: its container is not
        running, or, for a node ring, the node started after it. A container
        that joined a node's network namespace keeps running in the orphaned
        namespace when the node stops, and sees no packet of the restarted node."""
        state = self.executor.run(["docker", "inspect", "-f", "{{.State.Running}} {{.State.StartedAt}}", ring.container], timeout_s=30)
        running, _, ring_started = state.stdout.strip().partition(" ")
        if state.rc != 0 or running != "true":
            return True
        node = ring.node()
        if node is None:
            return False
        res = self.executor.run(["docker", "inspect", "-f", "{{.State.StartedAt}}", node], timeout_s=30)
        node_ns, ring_ns = started_ns(res.stdout), started_ns(ring_started)
        return res.rc == 0 and node_ns is not None and ring_ns is not None and node_ns > ring_ns

    def revive(self) -> int:
        """Start again every stale ring, as its next incarnation; the count
        started. A ring that does not start (its node still down) is tried
        again at the next call."""
        n = 0
        for ring in self.rings:
            if not self._stale(ring):
                continue
            self.executor.run(["docker", "rm", "-f", ring.container], timeout_s=60)
            ring.incarnation += 1
            if self._launch(ring).rc == 0:
                n += 1
        return n

    def keep(self, event: str, opened_ms: int) -> int:
        """Start again any ring found stopped, then copy every ring file written
        since the window opened; the count copied."""
        self.revive()
        n = 0
        since = (opened_ms - KEEP_MARGIN_MS) / 1000
        for ring in self.rings:
            dest = self.run_dir / "pcap" / "windows" / event / ring.source
            for f in sorted(ring.directory.glob(RING_GLOB)):
                if f.stat().st_mtime >= since:
                    dest.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(f, dest / f.name)
                    n += 1
        return n

    def stop(self) -> None:
        """Start again any ring found stopped, then stop every ring (tcpdump
        flushes) and hand its files to the runner's user."""
        self.revive()
        for ring in self.rings:
            self.executor.run(["docker", "stop", "-t", "5", ring.container], timeout_s=60)
            self.executor.run(
                ["docker", "run", "--rm", "-v", f"{ring.directory}:/cap", IMAGE, "chown", "-R", f"{os.getuid()}:{os.getgid()}", "/cap"],
                timeout_s=60,
            )
            self.executor.run(["docker", "rm", "-f", ring.container], timeout_s=60)
