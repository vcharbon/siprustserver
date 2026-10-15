"""`hakit run`: one profile against the lab, from preflight to `verdict.json`.

Start the `run` streams, fire the schedule, drain, collect each generator's
counter and its records, and compute the verdict from the run directory. An
interruption heals the event in progress and removes the generators.
"""

from __future__ import annotations

import json
import signal
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from .check import profile_errors
from .classes import run_counts
from .clock import Clock
from .collect import collect_cdrs
from .content import Content
from .events import KEEPALIVE_MS, EventJournal
from .executor import Executor
from .freshness import Freshness
from .incident_exec import EventRunner
from .injection import render
from .jsonval import Json
from .lab import Lab
from .model import Incident
from .offschedule import OffSchedule, clear_active, mark_active
from .paths import REPO_ROOT
from .preflight import earlier_open_events, host_facts, limit_errors
from .profile import Profile
from .schedule import PlannedEvent, plan
from .schemas import SchemaSet
from .sipp_driver import SippStream
from .summary import render as render_summary
from .verdict import compute
from .vlogs import VictoriaLogs
from .vm import QueryFailed, VictoriaMetrics

SHIP_WAIT_S = 60


class RunRefused(Exception):
    pass


def _terminate(signum: int, _frame: object) -> None:
    raise SystemExit(128 + signum)


def _raise_on_termination() -> Callable[[], None]:
    """SIGTERM and SIGHUP raise, so the run heals its open event; the returned
    call restores the previous handlers."""
    previous = {s: signal.signal(s, _terminate) for s in (signal.SIGTERM, signal.SIGHUP)}

    def restore() -> None:
        for s, h in previous.items():
            signal.signal(s, h)

    return restore


def _dump(p: Path, doc: object) -> None:
    p.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")


@dataclass
class _Live:
    streams: list[SippStream] = field(default_factory=lambda: [])
    died: set[str] = field(default_factory=lambda: set())
    loading: bool = False
    event: tuple[PlannedEvent, list[Incident]] | None = None
    load_start_ms: int = 0
    off: OffSchedule | None = None
    fresh: Freshness | None = None


class Runner:
    def __init__(
        self,
        content: Content,
        schemas: SchemaSet,
        lab: Lab,
        executor: Executor,
        clock: Clock,
        results: Path,
        say: Callable[[str], None] = print,
    ) -> None:
        self.content = content
        self.schemas = schemas
        self.lab = lab
        self.executor = executor
        self.clock = clock
        self.results = results
        self.say = say
        self.vm = VictoriaMetrics(executor, lab.vm_url)
        self.vlogs = VictoriaLogs(executor, lab.vlogs_url)

    def refusals(self, profile: Profile) -> list[str]:
        errs = profile_errors(profile, self.content)
        for s in profile.streams:
            if s.driver != "sipp":
                errs.append(f"stream {s.name!r}: driver {s.driver!r} is not implemented by this runner")
            if s.start != "run":
                errs.append(f"stream {s.name!r}: `start: incident` streams are not implemented by this runner")
        for ev in plan(profile, self.content.incidents):
            for name in ev.spec.incidents:
                inc = self.content.incidents[name]
                if inc.kind != "fault":
                    errs.append(f"incident {name!r}: load incidents are not implemented by this runner")
                missing = sorted(set(inc.requires) - set(self.lab.capabilities))
                if missing:
                    errs.append(f"incident {name!r} requires {missing}, which the lab lacks")
        if len(profile.streams) > self.lab.sipp_ip_slots:
            errs.append(f"{len(profile.streams)} streams exceed the lab's {self.lab.sipp_ip_slots} generator slots")
        if profile.cdr_join and "cdr" not in self.lab.capabilities:
            errs.append("cdr_join needs the lab capability `cdr` (a CDR consumer shipping observations)")
        return errs

    def run(self, profile_name: str) -> Path:
        profile = self.content.profiles.get(profile_name)
        if profile is None:
            raise RunRefused(f"profile {profile_name!r} is not on the search path")
        errs = self.refusals(profile)
        if errs:
            raise RunRefused("\n".join(errs))
        stale = earlier_open_events(self.results)
        if stale:
            raise RunRefused(f"events of an earlier run are still open ({', '.join(stale)}); run `hakit cleanup`")
        facts = host_facts(self.executor, self.results)
        errs = limit_errors(profile.preflight, facts)
        if errs:
            raise RunRefused("host outside the profile's preflight limits: " + "; ".join(errs))
        try:
            held = self.vm.scalar(self.lab.leftover_calls_query)
        except QueryFailed as e:
            raise RunRefused(f"leftover_calls: {e}") from e
        if held > 0:
            raise RunRefused(f"leftover_calls: the system under test holds {held:g} calls")

        started = self.clock.now_ms()
        stamp = datetime.fromtimestamp(started / 1000, UTC).strftime("%Y%m%dT%H%M%SZ")
        run = f"{profile.name}-{stamp}"
        run_dir = self.results / run
        run_dir.mkdir(parents=True)
        self._write_inputs(run_dir, run, profile, facts, held, started)
        journal = EventJournal(run, run_dir, self.clock, self.vm)
        live = _Live()
        integrity: list[Json] = []

        def wait_until(target_ms: int) -> None:
            self._wait(target_ms, journal, live, integrity, run)

        events = EventRunner(run, run_dir, self.lab, self.executor, self.clock, journal, wait_until)
        live.off = OffSchedule(
            run_dir, profile, self.content, journal, events, self.clock, lambda: live.load_start_ms, self.say
        )
        self.say(f"hakit: run {run} in {run_dir}")
        mark_active(run_dir)
        restore = _raise_on_termination()
        try:
            self._start_streams(run, run_dir, profile, live)
            load_start = self.clock.now_ms()
            live.load_start_ms = load_start
            live.fresh = self._freshness(run, profile, started)
            live.loading = True
            for ev in plan(profile, self.content.incidents):
                wait_until(load_start + int(ev.at_s * 1000))
                incs = [self.content.incidents[n] for n in ev.spec.incidents]
                live.event = (ev, incs)
                self.say(f"hakit: event {ev.event_id} (budget {ev.spec.budget})")
                outcome = events.run_event(ev, incs, "schedule")
                live.event = None
                if outcome.failed:
                    self.say(f"hakit: event {ev.event_id} failed; see events/{ev.event_id}/")
            wait_until(load_start + int(profile.duration_s * 1000))
            live.loading = False
            stream_docs = self._drain(profile, live, journal, run, started)
            close = journal.next_close_ms()
            while close is not None:
                wait_until(close)
                close = journal.next_close_ms()
        except BaseException:
            in_progress = live.event or live.off.current
            if in_progress is not None:
                ev, incs = in_progress
                events.heal_only(ev, incs)
            for s in live.streams:
                s.remove()
            raise
        finally:
            restore()
            clear_active(run_dir)
        for s in live.streams:
            s.remove()
        records, bad = self.vlogs.export(run, started - 3_600_000, self.clock.now_ms() + 3_600_000)
        with (run_dir / "records.jsonl").open("w") as fh:
            fh.writelines(json.dumps(r) + "\n" for r in records)
        unreadable: list[Json] = [{"kind": "generator_failure", "detail": f"unreadable record line: {b}"} for b in bad]
        integrity += unreadable
        if profile.cdr_join:
            integrity += collect_cdrs(self.vlogs, self.clock, run, run_dir, started)
        run_counts(self.content.classes, run_dir, self.executor)
        _dump(run_dir / "streams.json", stream_docs)
        with (run_dir / "integrity.jsonl").open("w") as fh:
            fh.writelines(json.dumps(i) + "\n" for i in integrity)
        meta = json.loads((run_dir / "run-meta.json").read_text())
        meta["ended_at_ms"] = self.clock.now_ms()
        meta["load_started_at_ms"] = live.load_start_ms
        _dump(run_dir / "run-meta.json", meta)
        self.write_verdict(run_dir)
        return run_dir

    def write_verdict(self, run_dir: Path) -> Json:
        v = compute(run_dir, self.schemas)
        errs = self.schemas.errors("verdict.v1", v)
        if errs:
            raise AssertionError(f"the verdict breaks verdict.v1: {errs[:3]}")
        _dump(run_dir / "verdict.json", v)
        (run_dir / "summary.md").write_text(render_summary(v))
        return v

    def _write_inputs(
        self, run_dir: Path, run: str, profile: Profile, facts: Json, held: float, started: int
    ) -> None:
        _dump(run_dir / "profile.json", profile.raw)
        (run_dir / "budgets").mkdir()
        (run_dir / "catalogs").mkdir()
        sched = profile.schedule
        names = {e.budget for e in (sched.cycle + sched.at if sched else [])}
        names |= {str(d.get("budget")) for d in profile.defaults.values()}
        for name in sorted(names):
            _dump(run_dir / "budgets" / f"{name}.json", self.content.budgets[name].raw)
        for name in profile.catalogs:
            _dump(run_dir / "catalogs" / f"{name}.json", self.content.catalogs[name].raw)
        head = self.executor.run(["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"], timeout_s=10)
        dirty = self.executor.run(["git", "-C", str(REPO_ROOT), "status", "--porcelain"], timeout_s=30)
        meta: Json = {
            "run": run,
            "profile": profile.name,
            "started_at_ms": started,
            "ended_at_ms": started,
            "tree": {"head": head.stdout.strip(), "dirty_files": len(dirty.stdout.splitlines())},
            "search_path": [str(p) for p in self.content.path],
            "host": facts,
            "sut_calls_at_start": held,
        }
        _dump(run_dir / "run-meta.json", meta)

    def _freshness(self, run: str, profile: Profile, started: int) -> Freshness:
        streams = [(s.name, s.cps, max(s.setup_deadline_s, s.ring_s + s.hold_s)) for s in profile.streams if s.start == "run"]
        return Freshness(run, self.vm, self.vlogs, self.clock, streams, self.lab.live_series, started)

    def _start_streams(self, run: str, run_dir: Path, profile: Profile, live: _Live) -> None:
        for slot, s in enumerate(profile.streams):
            sdir = run_dir / "streams" / s.name
            sdir.mkdir(parents=True)
            shape = self.content.shapes[s.shape]
            (sdir / "inject.csv").write_text(render(shape, self.content.catalogs[shape.catalog], self.lab.callees))
            stream = SippStream(run, s, slot, self.content.scenarios[s.scenario or ""], sdir, self.lab, self.executor)
            live.streams.append(stream)
            err = stream.start()
            if err is not None:
                raise RunRefused(err)
            self.say(f"hakit: stream {s.name} up ({s.cps:g} cps, shape {s.shape})")

    def _wait(self, target_ms: int, journal: EventJournal, live: _Live, integrity: list[Json], run: str) -> None:
        while True:
            journal.tick()
            if live.loading:
                for s in live.streams:
                    if s.stream.name not in live.died and not s.running():
                        at = self.clock.now_ms()
                        live.died.add(s.stream.name)
                        integrity.append(
                            {"kind": "stream_died", "stream": s.stream.name, "at_ms": at, "detail": "generator exited"}
                        )
                        if live.off is not None:
                            live.off.detected("stream_died")
                if live.fresh is not None:
                    integrity += live.fresh.tick()
                if live.off is not None:
                    live.off.poll()
            now = self.clock.now_ms()
            if now >= target_ms:
                return
            step = min(target_ms - now, KEEPALIVE_MS)
            close = journal.next_close_ms()
            if close is not None and close > now:
                step = min(step, close - now)
            self.clock.sleep(step / 1000)

    def _drain(self, profile: Profile, live: _Live, journal: EventJournal, run: str, started: int) -> list[Json]:
        for s in live.streams:
            s.drain()
        deadline = self.clock.now_ms() + int(profile.drain_s * 1000)
        while any(s.running() for s in live.streams) and self.clock.now_ms() < deadline:
            self._wait(min(deadline, self.clock.now_ms() + 2000), journal, live, [], run)
        docs: list[Json] = []
        for s in live.streams:
            drained = not s.running()
            if not drained:
                s.kill()
            local = s.local_records()
            ship_deadline = self.clock.now_ms() + SHIP_WAIT_S * 1000
            shipped = self.vlogs.count(run, s.stream.name, started - 3_600_000, self.clock.now_ms() + 3_600_000)
            while shipped != local and self.clock.now_ms() < ship_deadline:
                self.clock.sleep(2)
                shipped = self.vlogs.count(run, s.stream.name, started - 3_600_000, self.clock.now_ms() + 3_600_000)
            c = s.counters()
            docs.append(
                {
                    "stream": s.stream.name,
                    "shape": s.stream.shape,
                    "start": s.stream.start,
                    "cps": s.stream.cps,
                    "calls": c.calls_created,
                    "not_placed": 0,
                    "reconciled": True,
                    "drained": drained,
                    "exit_code": s.exit_code(),
                    "local_records": local,
                    "shipped_records": shipped,
                }
            )
        return docs
