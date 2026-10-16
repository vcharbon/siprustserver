"""A loadgen stream: one container of a product binary built on the loadgen
library, on the generator network at the stream's slot address.

The stream's `params` name the product: `image` (the container image) and
`command` (the binary in it). Every other parameter is one more flag of the
binary, `--<name>` with `_` spelled `-` (`true` a bare flag, `false` none);
the flags the runner sets itself are refused (`RESERVED`). loadgen plays the
caller and every callee leg of its calls, writes one call-outcome record per
call into `records.jsonl` of the stream directory and ships it to
VictoriaLogs.

A drain is SIGUSR1: loadgen places no new call, its calls in flight end, it
exits 0 and leaves the last `/metrics` body in `metrics.prom` of the stream
directory, which holds the counters the stream is reconciled against.
"""

from __future__ import annotations

import math
import re
from pathlib import Path

from .executor import Executor
from .generator import Tally
from .lab import Lab, resolve_target
from .model import Scalar
from .profile import Stream

# Retransmission bound of a request without an answer (64 * T1).
TXN_BOUND_S = 32
# The caller binds this port, the callee legs the two after it.
BASE_PORT = 6000
METRICS_PORT = 9300
# A stream runs until it is drained: its duration only bounds a run nobody drains.
DURATION_BOUND_S = 864_000
FINAL_METRICS = "metrics.prom"

# Flags the runner sets from the profile and the lab.
RESERVED = frozenset(
    {
        "target",
        "bind_ip",
        "base_port",
        "scenario",
        "cps",
        "duration",
        "max_in_flight",
        "recv_timeout_ms",
        "ring_delay_ms",
        "talk_time_ms",
        "run",
        "stream",
        "call_id_tag",
        "records_file",
        "records_url",
        "out_dir",
        "metrics_addr",
        "load_profile",
    }
)
PRODUCT = ("image", "command")

_SAMPLE = re.compile(r"^(?P<name>[a-z_]+)\{(?P<labels>[^}]*)\}\s+(?P<value>\S+)$")


def worst_call_s(stream: Stream) -> float:
    """The longest a call can take: its setup deadline, its ring and hold, and
    a teardown request that gets no final."""
    return stream.setup_deadline_s + stream.ring_s + stream.hold_s + TXN_BOUND_S


def param_errors(stream: Stream) -> list[str]:
    """What the stream's `params` state wrong for a loadgen stream."""
    errs: list[str] = []
    for key in PRODUCT:
        if not isinstance(stream.params.get(key), str) or not stream.params.get(key):
            errs.append(f"params.{key} must name the product's {key}")
    for key in sorted(set(stream.params) & RESERVED):
        errs.append(f"params.{key} is a flag the runner sets itself")
    return errs


def _flag(name: str, value: Scalar) -> list[str]:
    """`--name=value` (`=`, so a value starting with `-` stays a value)."""
    flag = "--" + name.replace("_", "-")
    if isinstance(value, bool):
        return [flag] if value else []
    return [f"{flag}={value}"]


def parse_metrics(text: str, stream: str) -> Tally:
    """The stream's counters from a `/metrics` body: calls offered, not placed,
    and failed by the generator, each summed over its labels."""
    sums: dict[str, int] = {}
    for line in text.splitlines():
        m = _SAMPLE.match(line.strip())
        if m is None or f'stream="{stream}"' not in m.group("labels"):
            continue
        try:
            value = int(float(m.group("value")))
        except ValueError:
            continue
        sums[m.group("name")] = sums.get(m.group("name"), 0) + value
    offered = sums.get("loadgen_calls_offered_total")
    if offered is None:
        return Tally(None)
    return Tally(
        offered,
        sums.get("loadgen_not_placed_total", 0),
        sums.get("loadgen_generator_failures_total", 0),
    )


class LoadgenStream:
    def __init__(
        self,
        run: str,
        stream: Stream,
        slot: int,
        stream_dir: Path,
        lab: Lab,
        executor: Executor,
        call_id_tag: str = "",
    ) -> None:
        """`call_id_tag` follows the run id in every Call-ID: two generators of
        one stream in one run (two firings of a load incident) each carry their
        own, so no Call-ID repeats."""
        self.run = run
        self.call_id_tag = call_id_tag
        self.stream = stream
        self.slot = slot
        self.dir = stream_dir
        self.lab = lab
        self.executor = executor
        self.name = f"hakit-{stream.name}"
        self.ip = lab.sipp_ip(slot)

    @property
    def records_path(self) -> Path:
        return self.dir / "records.jsonl"

    def max_concurrent(self) -> int:
        """loadgen's in-flight cap: every call the peak rate places, each at its
        worst length (ADR-0049 §3.12)."""
        s = self.stream
        if s.max_concurrent is not None:
            return s.max_concurrent
        return int(math.ceil(s.peak_cps * worst_call_s(s))) + 10

    def argv(self, target: str | None = None, bind_ip: str | None = None, records_url: str | None = None) -> list[str]:
        """The binary's command line: by default for a container on the
        generator network at the stream's slot address."""
        s = self.stream
        argv = [
            str(s.params["command"]),
            "--target",
            target or self.lab.sip_target,
            "--bind-ip",
            bind_ip or self.ip,
            "--base-port",
            str(BASE_PORT),
            "--scenario",
            s.shape,
            "--cps",
            f"{s.cps:g}",
            "--duration",
            str(DURATION_BOUND_S),
            "--max-in-flight",
            str(self.max_concurrent()),
            "--recv-timeout-ms",
            str(int(s.setup_deadline_s * 1000)),
            "--ring-delay-ms",
            str(int(s.ring_s * 1000)),
            "--talk-time-ms",
            str(int(s.hold_s * 1000)),
            "--run",
            self.run,
            "--stream",
            s.name,
            "--records-file",
            "/stats/records.jsonl",
            "--records-url",
            records_url or self.lab.vlogs_url_from_generators,
            "--out-dir",
            "/stats",
            "--metrics-addr",
            f"0.0.0.0:{METRICS_PORT}",
        ]
        if self.call_id_tag:
            # `=`: a tag starts with `-`, which would read as a flag.
            argv.append(f"--call-id-tag={self.call_id_tag}")
        for k, v in sorted(s.params.items()):
            if k not in PRODUCT:
                argv += _flag(k, v)
        return argv

    def shape_check(self) -> str | None:
        """Ask the product image which shapes it registers, with this stream's
        flags: an error text when the image or a flag is refused or the
        stream's shape is not registered."""
        res = self.executor.run(
            ["docker", "run", "--rm", str(self.stream.params["image"]), *self.argv(), "--list-shapes"],
            timeout_s=120,
        )
        if res.rc != 0:
            return f"stream {self.stream.name!r}: the loadgen image refused its flags: {res.stderr.strip()[:300]}"
        if self.stream.shape not in res.stdout.split():
            return f"stream {self.stream.name!r}: shape {self.stream.shape!r} is not registered in its loadgen image"
        return None

    def start(self) -> str | None:
        target, err = resolve_target(self.lab, self.executor, self.stream.target)
        if target is None:
            return f"loadgen of {self.stream.name} did not start: {err}"
        labels = ["--label", f"hakit-run={self.run}", "--label", f"hakit-stream={self.stream.name}"]
        self.remove()
        res = self.executor.run(
            ["docker", "run", "-d", "--name", self.name, *labels]
            + ["--network", self.lab.sipp_network, "--ip", self.ip, "--cpus", "4", "--memory", "1536m"]
            + ["-v", f"{self.dir}:/stats", str(self.stream.params["image"]), *self.argv(target=target)],
            timeout_s=60,
        )
        if res.rc != 0:
            return f"loadgen of {self.stream.name} did not start: {res.stderr.strip()[:300]}"
        return None

    def _inspect(self, fmt: str) -> str | None:
        res = self.executor.run(["docker", "inspect", "-f", fmt, self.name], timeout_s=30)
        return res.stdout.strip() if res.rc == 0 else None

    def running(self) -> bool:
        return self._inspect("{{.State.Running}}") == "true"

    def exit_code(self) -> int | None:
        raw = self._inspect("{{.State.ExitCode}}")
        try:
            return int(raw) if raw is not None else None
        except ValueError:
            return None

    def clean_exit(self, code: int | None) -> bool:
        """A drained loadgen exits 0 once its records are sealed."""
        return code == 0

    def drain(self) -> bool:
        return self.executor.run(["docker", "kill", "--signal", "USR1", self.name], timeout_s=30).rc == 0

    def kill(self) -> bool:
        return self.executor.run(["docker", "kill", self.name], timeout_s=30).rc == 0

    def set_rate(self, cps: float) -> bool:
        """loadgen's `POST /rate?cps=`."""
        res = self.executor.http("POST", f"http://{self.ip}:{METRICS_PORT}/rate?cps={cps:g}", b"", timeout_s=10)
        return res.status == 200

    def tally(self) -> Tally:
        """The counters of the last `/metrics` body, which loadgen leaves in the
        stream directory as it exits; the live endpoint while it runs."""
        text = self.executor.read_text(self.dir / FINAL_METRICS)
        if text is None:
            res = self.executor.http("GET", f"http://{self.ip}:{METRICS_PORT}/metrics", timeout_s=10)
            text = res.body if res.status == 200 else ""
        return parse_metrics(text, self.stream.name)

    def local_records(self) -> int:
        text = self.executor.read_text(self.records_path) or ""
        return sum(1 for ln in text.splitlines() if ln.strip())

    def remove(self) -> None:
        self.executor.run(["docker", "rm", "-f", self.name], timeout_s=60)
