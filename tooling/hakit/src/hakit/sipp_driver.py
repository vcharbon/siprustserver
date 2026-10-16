"""A SIPp stream: one UAC container and its exporter twin on the generator network.

The twin starts first and holds the stream's address; the UAC joins its network
namespace, so the twin keeps shipping the last records after the UAC exits.
The UAC writes one call-outcome record per call into `records.jsonl` of the
stream directory (its `<log>` action); the twin ships each line to
VictoriaLogs and serves the stat counters (ADR-0049 §4.2).

A drain is SIGUSR1: SIPp places no new call and exits once its calls are done.
"""

from __future__ import annotations

import math
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from .executor import Executor
from .lab import Lab
from .paths import SIPP_EXPORTER_DIR
from .profile import Stream
from .scenario_lint import key_defaults

# Retransmission bound of an INVITE or BYE without an answer (64 * T1).
TXN_BOUND_S = 32
# Tail of SIPp's error file the exporter twin keeps: every 1 s deadline check
# logs a line there, and the file is not rotated, since -ringbuffer_* would
# rotate the record file too.
ERROR_KEEP_BYTES = 16 * 1024 * 1024
# Every error entry but the scenario's own receive timer, kept whole for the
# run (the tail above holds minutes at a high rate), bounded by this tail.
ERROR_KEPT_FILE = "/stats/errors.kept.log"
ERROR_KEPT_BYTES = 256 * 1024 * 1024
# Margin of the global `-recv_timeout` above every timeout of the scenario.
RECV_BACKSTOP_MARGIN_S = 60
# The `-key`s every stream passes; a scenario's other keys come from the stream's params.
DRIVER_KEYS = ("run", "stream", "shape", "hold_ms", "setup_ms")


def _key_number(stream: Stream, defaults: Mapping[str, str], key: str) -> float:
    raw = stream.params.get(key, defaults.get(key, "0"))
    try:
        return float(raw)
    except (TypeError, ValueError):
        return 0.0


def worst_call_s(stream: Stream, defaults: Mapping[str, str] | None = None) -> float:
    """The longest a call of the base UAC scenario can take: the setup deadline
    (or Timer B, when later), then either the hold (or a flood) with each
    re-INVITE's transaction bound, or the 64*T1 wait for the INVITE's final
    after a CANCEL, then a BYE that gets no final. `defaults` are the
    scenario's declared keys, under the stream's params."""
    d = defaults or {}
    flood_s = _key_number(stream, d, "flood_count") * _key_number(stream, d, "flood_gap_ms") / 1000
    flooding = str(stream.params.get("abuse", d.get("abuse", "none"))).endswith("_flood")
    hold = max(stream.hold_s, flood_s if flooding else 0.0) + _key_number(stream, d, "reinvites") * TXN_BOUND_S
    return max(stream.setup_deadline_s, TXN_BOUND_S) + max(hold, TXN_BOUND_S) + TXN_BOUND_S


@dataclass(frozen=True)
class Counters:
    calls_created: int | None
    current_calls: int | None


def parse_stat_csv(text: str | None) -> Counters:
    if not text:
        return Counters(None, None)
    lines = [ln for ln in text.splitlines() if ln.strip()]
    if len(lines) < 2:
        return Counters(None, None)
    header = lines[0].split(";")
    row = lines[-1].split(";")

    def col(name: str) -> int | None:
        if name not in header:
            return None
        i = header.index(name)
        try:
            return int(row[i]) if i < len(row) else None
        except ValueError:
            return None

    return Counters(col("TotalCallCreated"), col("CurrentCall"))


class SippStream:
    def __init__(
        self,
        run: str,
        stream: Stream,
        slot: int,
        scenario: Path,
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
        self.scenario = scenario
        self.dir = stream_dir
        self.lab = lab
        self.executor = executor
        self.uac = f"hakit-{stream.name}"
        self.twin = f"hakit-{stream.name}-exporter"

    @property
    def records_path(self) -> Path:
        return self.dir / "records.jsonl"

    def recv_timeout_ms(self) -> int:
        s = self.stream
        return int((worst_call_s(s, key_defaults(self.scenario)) + RECV_BACKSTOP_MARGIN_S) * 1000)

    def max_concurrent(self) -> int:
        """SIPp's call limit: every call the peak rate places, each at its worst
        length, so the limit is reached only when the system under test holds
        calls past every deadline of the scenario (ADR-0049 §3.12)."""
        s = self.stream
        if s.max_concurrent is not None:
            return s.max_concurrent
        return int(math.ceil(s.peak_cps * worst_call_s(s))) + 10

    def sipp_argv(self) -> list[str]:
        s = self.stream
        argv = [
            "sipp",
            self.lab.sip_target,
            "-sf",
            f"/scenarios/{self.scenario.name}",
            "-inf",
            "/stats/inject.csv",
            "-key",
            "run",
            self.run,
            "-key",
            "stream",
            s.name,
            "-key",
            "shape",
            s.shape,
            "-key",
            "hold_ms",
            str(int(s.hold_s * 1000)),
            "-key",
            "setup_ms",
            str(int(s.setup_deadline_s * 1000)),
            "-cid_str",
            f"%u-%p-{self.run}{self.call_id_tag}@%s",
            "-i",
            self.lab.sipp_ip(self.slot),
            "-p",
            "5060",
            "-r",
            f"{s.cps:g}",
            "-rp",
            "1000",
            "-l",
            str(self.max_concurrent()),
            "-recv_timeout",
            str(self.recv_timeout_ms()),
            "-trace_logs",
            "-log_file",
            "/stats/records.jsonl",
            "-trace_err",
            "-error_file",
            "/stats/errors.log",
            "-trace_stat",
            "-stf",
            "/stats/stat.csv",
            "-fd",
            "1",
        ]
        for k, v in sorted(key_defaults(self.scenario).items()):
            if k not in DRIVER_KEYS:
                argv += ["-key", k, v]
        for k, v in sorted(s.params.items()):
            argv += ["-key", k, str(v)]
        return argv

    def start(self) -> str | None:
        """Start the twin then the UAC; an error text when either refused to start."""
        ip = self.lab.sipp_ip(self.slot)
        labels = ["--label", f"hakit-run={self.run}", "--label", f"hakit-stream={self.stream.name}"]
        self.remove()
        twin = self.executor.run(
            ["docker", "run", "-d", "--name", self.twin, *labels]
            + ["--network", self.lab.sipp_network, "--ip", ip, "--cpus", "0.5", "--memory", "256m"]
            + ["-v", f"{self.dir}:/stats", "-v", f"{SIPP_EXPORTER_DIR}:/exporter:ro"]
            + ["-e", "SIPP_STAT_FILE=/stats/stat.csv", "-e", f"SIPP_SCENARIO={self.scenario.name}"]
            + ["-e", f"SIPP_ROLE={self.stream.name}", "-e", f"SIPP_JOB={self.uac}"]
            + ["-e", f"SIPP_STREAM={self.stream.name}", "-e", f"SIPP_SHAPE={self.stream.shape}"]
            + ["-e", "SIPP_RECORD_FILE=/stats/records.jsonl", "-e", f"SIPP_RUN={self.run}"]
            + ["-e", f"SIPP_ERROR_KEEP_BYTES={ERROR_KEEP_BYTES}"]
            + ["-e", f"SIPP_ERROR_KEPT_FILE={ERROR_KEPT_FILE}", "-e", f"SIPP_ERROR_KEPT_BYTES={ERROR_KEPT_BYTES}"]
            + ["-e", f"SIPP_RECORD_URL={self.lab.vlogs_url_from_generators}"]
            + [self.lab.sipp_image, "python3", "/exporter/sipp_stat_exporter.py"],
            timeout_s=60,
        )
        if twin.rc != 0:
            return f"exporter twin of {self.stream.name} did not start: {twin.stderr.strip()[:300]}"
        uac = self.executor.run(
            ["docker", "run", "-d", "--name", self.uac, *labels]
            + ["--network", f"container:{self.twin}", "--cpus", "8", "--memory", "1536m"]
            + ["-v", f"{self.scenario.parent}:/scenarios:ro", "-v", f"{self.dir}:/stats", "-w", "/stats"]
            + [self.lab.sipp_image, *self.sipp_argv()],
            timeout_s=60,
        )
        if uac.rc != 0:
            return f"SIPp UAC of {self.stream.name} did not start: {uac.stderr.strip()[:300]}"
        return None

    def _inspect(self, name: str, fmt: str) -> str | None:
        res = self.executor.run(["docker", "inspect", "-f", fmt, name], timeout_s=30)
        return res.stdout.strip() if res.rc == 0 else None

    def running(self) -> bool:
        return self._inspect(self.uac, "{{.State.Running}}") == "true"

    def exit_code(self) -> int | None:
        raw = self._inspect(self.uac, "{{.State.ExitCode}}")
        try:
            return int(raw) if raw is not None else None
        except ValueError:
            return None

    def drain(self) -> bool:
        """Signal SIPp to place no new call and exit once its calls end; whether
        docker delivered the signal. Never sent twice: a second SIGUSR1 makes
        SIPp exit at once."""
        return self.executor.run(["docker", "kill", "--signal", "USR1", self.uac], timeout_s=30).rc == 0

    def kill(self) -> bool:
        return self.executor.run(["docker", "kill", self.uac], timeout_s=30).rc == 0

    def counters(self) -> Counters:
        return parse_stat_csv(self.executor.read_text(self.dir / "stat.csv"))

    def local_records(self) -> int:
        text = self.executor.read_text(self.records_path) or ""
        return sum(1 for ln in text.splitlines() if ln.strip())

    def remove(self) -> None:
        self.executor.run(["docker", "rm", "-f", self.uac, self.twin], timeout_s=60)
