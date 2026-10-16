"""The generators of a run and their lifecycle (ADR-0049 §2.8, §4.5).

A `run` stream starts with the load and drains with it. An `incident` stream
runs once per firing of the load incident whose event names it: each firing is
its own generator, started at inject in its own directory under the stream's,
its Call-IDs tagged with the event so two firings never share one. At heal,
`drain` tells the UAC to place no new call and waits for its calls to end;
`abandon` kills it with its calls in flight. Either way the records it wrote
are shipped and its counter read before the generator is removed, so the next
firing starts clean. A docker generator works in its working directory
(workdir.py), collected into its stream directory once it is removed. A
stream's `streams.json` entry sums its firings; an abandoned stream is left
out of the reconciliation.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from .clock import Clock
from .content import Content
from .executor import Executor
from .generator import Generator
from .injection import render
from .jsonval import Json
from .lab import Lab
from .loadgen import LoadgenStream
from .model import Incident
from .pod_loadgen import PodLoadgenStream
from .profile import Profile, Stream
from .reconcile import integrity
from .schedule import PlannedEvent
from .sipp_driver import SippStream
from .vlogs import VictoriaLogs
from .workdir import WorkDir, work_dir

SHIP_WAIT_S = 60
POLL_MS = 2000
# VictoriaLogs is queried around the run's own span with this margin.
QUERY_MARGIN_MS = 3_600_000

type Waiter = Callable[[int], None]


@dataclass
class Firing:
    """One generator of a stream: the whole load for a `run` stream, one load
    incident's span for an `incident` one."""

    event: str | None
    gen: Generator
    workdir: WorkDir
    stop: str = "drain"
    drained: bool = False
    calls: int | None = None
    not_placed: int = 0
    generator_failures: int | None = None
    local: int = 0
    exit_code: int | None = None
    collected: bool = False


class StreamSet:
    def __init__(
        self,
        run: str,
        run_dir: Path,
        profile: Profile,
        content: Content,
        lab: Lab,
        executor: Executor,
        clock: Clock,
        vlogs: VictoriaLogs,
        started_ms: int,
    ) -> None:
        self.run = run
        self.run_dir = run_dir
        self.profile = profile
        self.content = content
        self.lab = lab
        self.executor = executor
        self.clock = clock
        self.vlogs = vlogs
        self.started_ms = started_ms
        self.firings: dict[str, list[Firing]] = {s.name: [] for s in profile.streams}
        self.active: dict[str, Firing] = {}

    def _slot(self, stream: Stream) -> int:
        return next(i for i, s in enumerate(self.profile.streams) if s.name == stream.name)

    def _firing(self, stream: Stream, sdir: Path, tag: str, event: str | None) -> Firing:
        """A new generator of `stream` in the stream directory `sdir`; a pod
        generator copies its files out itself and needs no working directory."""
        sdir.mkdir(parents=True)
        if stream.driver == "loadgen" and stream.placement == "pod":
            gen = PodLoadgenStream(self.run, stream, self._slot(stream), sdir, self.lab, self.executor, call_id_tag=tag)
            return Firing(event, gen, WorkDir(sdir, sdir))
        wd = work_dir(self.lab.generator_scratch, self.run_dir, sdir)
        if stream.driver == "loadgen":
            wd.prepare(self.executor, [])
            gen = LoadgenStream(self.run, stream, self._slot(stream), wd.work, self.lab, self.executor, call_id_tag=tag)
            return Firing(event, gen, wd)
        shape = self.content.shapes[stream.shape]
        (sdir / "inject.csv").write_text(render(shape, self.content.catalogs[shape.catalog], self.lab.callees))
        wd.prepare(self.executor, [sdir / "inject.csv"])
        scenario = self.content.scenarios[stream.scenario or ""]
        gen = SippStream(self.run, stream, self._slot(stream), scenario, wd.work, self.lab, self.executor, call_id_tag=tag)
        return Firing(event, gen, wd)

    def _remove(self, f: Firing) -> None:
        """Remove the firing's generator, then collect its working directory once."""
        f.gen.remove()
        if not f.collected:
            f.collected = f.workdir.collect(self.executor)

    def start_run_streams(self) -> str | None:
        """Start every `run` stream; the refusal of the first that does not start."""
        for s in self.profile.streams:
            if s.start != "run":
                continue
            f = self._firing(s, self.run_dir / "streams" / s.name, "", None)
            self.firings[s.name].append(f)
            self.active[s.name] = f
            err = f.gen.start()
            if err is not None:
                return err
        return None

    def set_rate(self, name: str, cps: float) -> bool | None:
        """Change the rate of the running generator of `name`; None when none runs."""
        f = self.active.get(name)
        return None if f is None else f.gen.set_rate(cps)

    def live(self) -> list[Generator]:
        """The generators due to be running: a stream is out of this list from the
        instant its stop begins."""
        return [f.gen for f in self.active.values()]

    def start_incident(self, name: str, event: str) -> tuple[bool, Json]:
        """Start one firing of the `incident` stream `name` for `event`."""
        stream = self.profile.stream(name)
        if stream is None or stream.start != "incident" or name in self.active:
            return False, {"stream": name, "error": "not an idle `start: incident` stream of the profile"}
        n = len(self.firings[name]) + 1
        f = self._firing(stream, self.run_dir / "streams" / name / event, f"-{event}", event)
        self.firings[name].append(f)
        self.active[name] = f
        err = f.gen.start()
        facts: Json = {"stream": name, "firing": n, "cps": stream.cps, "shape": stream.shape}
        if err is not None:
            del self.active[name]
            self._remove(f)
            return False, {**facts, "error": err}
        return True, facts

    def stop_incident(self, name: str, mode: str, deadline_ms: int, wait: Waiter) -> tuple[bool, Json]:
        """End the running firing of `name`: `drain`, `abandon`, or `remove` (an
        interrupted run: the pair is removed at once). The firing's facts, its
        exit instant among them; false when docker refused the signal or a
        drain did not finish by `deadline_ms` (the UAC is then killed). The
        records are shipped and the counter read before the pair is removed."""
        f = self.active.pop(name, None)
        if f is None:
            return True, {"stream": name, "stop": mode, "running": False}
        if mode == "remove":
            self._remove(f)
            return True, {"stream": name, "stop": mode}
        stopped_at = self.clock.now_ms()
        f.stop = mode
        signalled = f.gen.kill() if mode == "abandon" else f.gen.drain()
        while signalled and f.gen.running() and self.clock.now_ms() < deadline_ms:
            wait(min(deadline_ms, self.clock.now_ms() + POLL_MS))
        exited_at = self.clock.now_ms()
        f.drained = signalled and mode == "drain" and not f.gen.running()
        ok = f.drained or (signalled and mode == "abandon")
        if not ok:
            f.gen.kill()
        shipped = self._settle(f, wait)
        if f.drained and not f.gen.clean_exit(f.exit_code):
            f.drained = ok = False
        self._remove(f)
        facts: Json = {
            "stream": name,
            "stop": mode,
            "signalled": signalled,
            "drained": f.drained,
            "exited_at_ms": exited_at,
            "teardown_ms": exited_at - stopped_at,
            "calls": f.calls,
            "records": f.local,
            "shipped_records": shipped,
            "exit_code": f.exit_code,
        }
        return ok, facts

    def _settle(self, f: Firing, wait: Waiter) -> int | None:
        """Read the firing's counter and records, and wait (bounded) until
        VictoriaLogs holds every record the stream wrote so far."""
        tally = f.gen.tally()
        f.calls, f.not_placed, f.generator_failures = tally.calls, tally.not_placed, tally.generator_failures
        f.exit_code = f.gen.exit_code()
        f.local = f.gen.local_records()
        name = f.gen.stream.name
        expected = sum(x.local for x in self.firings[name])
        deadline = self.clock.now_ms() + SHIP_WAIT_S * 1000
        shipped = self._shipped(name)
        while shipped != expected and self.clock.now_ms() < deadline:
            wait(min(deadline, self.clock.now_ms() + POLL_MS))
            shipped = self._shipped(name)
        return shipped

    def _shipped(self, name: str) -> int | None:
        end = self.clock.now_ms() + QUERY_MARGIN_MS
        return self.vlogs.count(self.run, name, self.started_ms - QUERY_MARGIN_MS, end)

    def drain(self, deadline_ms: int, wait: Waiter) -> None:
        """The end of the load: drain every generator still running and settle
        each, killing a UAC still running at `deadline_ms`."""
        running = list(self.active.values())
        self.active.clear()
        signalled = [f for f in running if f.gen.drain()]
        while any(f.gen.running() for f in signalled) and self.clock.now_ms() < deadline_ms:
            wait(min(deadline_ms, self.clock.now_ms() + POLL_MS))
        for f in running:
            f.drained = f in signalled and not f.gen.running()
            if not f.drained:
                f.gen.kill()
            self._settle(f, wait)
            f.drained = f.drained and f.gen.clean_exit(f.exit_code)

    def remove_all(self) -> list[Json]:
        """Remove every generator and collect its working directory; an
        integrity finding per firing whose files stayed in the scratch (its
        stat file, hence its stalls, is then missing from the run directory
        until `hakit cleanup` copies it out)."""
        for fs in self.firings.values():
            for f in fs:
                self._remove(f)
        left = [(name, f) for name, fs in self.firings.items() for f in fs if not f.collected]
        if self.lab.generator_scratch and not left:
            self.executor.run(["rm", "-r", "--", str(Path(self.lab.generator_scratch) / self.run)], timeout_s=120)
        return [
            integrity(
                "generator_failure",
                f"its files stayed in the generator scratch {f.workdir.work}: `hakit cleanup` copies them out",
                stream=name,
            )
            for name, f in left
        ]

    def docs(self) -> list[Json]:
        """`streams.json`: per stream its firings summed. An abandoned stream is
        not reconciled: its killed calls wrote no record."""
        out: list[Json] = []
        for s in self.profile.streams:
            fs = self.firings[s.name]
            counts = [f.calls for f in fs]
            calls = None if any(c is None for c in counts) else sum(c or 0 for c in counts)
            failures = [f.generator_failures for f in fs]
            failed = None if not fs or any(c is None for c in failures) else sum(c or 0 for c in failures)
            abandon = s.start == "incident" and self._stop_mode(s.name) == "abandon"
            shipped = self._shipped(s.name) if fs else 0
            out.append(
                {
                    "stream": s.name,
                    "shape": s.shape,
                    "start": s.start,
                    "cps": s.cps,
                    "call_limit": fs[0].gen.max_concurrent() if fs else None,
                    "calls": calls,
                    "not_placed": sum(f.not_placed for f in fs),
                    "reconciled": not abandon,
                    "firings": len(fs),
                    "drained": all(f.drained for f in fs if f.stop == "drain"),
                    "exit_code": fs[-1].exit_code if fs else None,
                    "local_records": sum(f.local for f in fs),
                    "shipped_records": shipped,
                    **({} if failed is None else {"generator_failures": failed}),
                }
            )
        return out

    def _stop_mode(self, stream: str) -> str:
        """The stop mode of the load incidents that name `stream` (one stream is
        driven by one load incident's mode; check.py refuses a mix)."""
        sched = self.profile.schedule
        events = [] if sched is None else sched.cycle + sched.at
        for ev in events:
            if ev.stream != stream:
                continue
            for name in ev.incidents:
                inc = self.content.incidents.get(name)
                if inc is not None and inc.kind == "load":
                    return inc.stop
        return "drain"


class StreamLoad:
    """The load steps of a run: the stream an event names, started at inject and
    stopped at heal by its incident's `stop`; an interrupted run removes it."""

    def __init__(self, streams: StreamSet, clock: Clock, wait: Waiter) -> None:
        self.streams = streams
        self.clock = clock
        self.wait = wait

    def start(self, inc: Incident, ev: PlannedEvent) -> tuple[bool, Json]:
        if ev.spec.stream is None:
            return False, {"error": f"load incident {inc.name!r} fired without a stream"}
        return self.streams.start_incident(ev.spec.stream, ev.event_id)

    def stop(self, inc: Incident, ev: PlannedEvent, interrupted: bool) -> tuple[bool, Json]:
        if ev.spec.stream is None:
            return True, {}
        mode = "remove" if interrupted else inc.stop
        deadline = self.clock.now_ms() + int(inc.timeouts.heal_s * 1000)
        return self.streams.stop_incident(ev.spec.stream, mode, deadline, self.wait)


class RemoveLoad:
    """The load steps of `hakit cleanup`: a crashed run's stream generator is
    removed, a container or a pod; nothing is started."""

    def __init__(self, executor: Executor, run: str, lab: Lab) -> None:
        self.executor = executor
        self.run = run
        self.lab = lab

    def start(self, inc: Incident, ev: PlannedEvent) -> tuple[bool, Json]:
        return False, {"error": "cleanup starts no stream"}

    def stop(self, inc: Incident, ev: PlannedEvent, interrupted: bool) -> tuple[bool, Json]:
        labels = ["--filter", f"label=hakit-run={self.run}", "--filter", f"label=hakit-stream={ev.spec.stream}"]
        ps = self.executor.run(["docker", "ps", "-aq", *labels], timeout_s=30)
        ids = ps.stdout.split()
        if ps.rc != 0:
            return False, {"stream": ev.spec.stream, "error": ps.stderr.strip()[:200]}
        if ids:
            rm = self.executor.run(["docker", "rm", "-f", *ids], timeout_s=60)
            if rm.rc != 0:
                return False, {"stream": ev.spec.stream, "error": rm.stderr.strip()[:200]}
        selector = f"hakit-run={self.run},hakit-stream={ev.spec.stream}"
        pods = self.executor.run(
            ["kubectl", "--context", self.lab.kube_context, "-n", self.lab.namespace, "delete", "pods", "-l", selector]
            + ["--grace-period=0", "--force", "--ignore-not-found"],
            timeout_s=120,
        )
        if pods.rc != 0:
            return False, {"stream": ev.spec.stream, "error": pods.stderr.strip()[:200]}
        return True, {"stream": ev.spec.stream, "stop": "remove", "removed": len(ids)}
