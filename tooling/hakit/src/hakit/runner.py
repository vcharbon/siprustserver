"""`hakit run`: one profile against the lab, from preflight to `verdict.json`.

Start the `run` streams, fire the schedule (a load incident starts and stops
its own stream), drain, collect each generator's counter and its records, and
compute the verdict from the run directory. An interruption heals the event in
progress and removes the generators.
"""

from __future__ import annotations

import json
import signal
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from .capture import CaptureFailed, Captures
from .check import profile_errors
from .classes import run_counts
from .clock import Clock
from .collect import collect_cdrs
from .content import Content
from .events import KEEPALIVE_MS, EventJournal
from .executor import Executor
from .freshness import Freshness
from .incident_exec import EventRunner
from .jsonl import JsonlLog, write_jsonl
from .jsonval import Json
from .lab import Lab, lab_capabilities
from .loadgen import LoadgenStream
from .model import Incident
from .offschedule import OffSchedule, clear_active, mark_active
from .paths import REPO_ROOT
from .preflight import earlier_open_events, host_facts, limit_errors
from .profile import Profile
from .rate_steps import RateSteps
from .schedule import PlannedEvent, plan
from .schemas import SchemaSet
from .streams import StreamLoad, StreamSet
from .summary import render as render_summary
from .verdict import compute_with_calls
from .verdict_series import verdict_lines
from .vlogs import VictoriaLogs
from .vm import QueryFailed, VictoriaMetrics


class RunRefused(Exception):
    pass


def _say(line: str) -> None:
    """A progress line, flushed: a detached run's log is read while it runs."""
    print(line, flush=True)


def _terminate(signum: int, _frame: object) -> None:
    """The first SIGTERM or SIGHUP ignores every later one, so the heal and the
    generators' removal it starts are never cut short: a signal sent to the
    run's process group reaches the runner a second time through a wrapper
    that forwards it."""
    for s in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(s, signal.SIG_IGN)
    raise SystemExit(128 + signum)


def _raise_on_termination() -> Callable[[], None]:
    """SIGTERM and SIGHUP raise, so the run heals its open event; the returned
    call restores the previous handlers."""
    previous = {s: signal.signal(s, _terminate) for s in (signal.SIGTERM, signal.SIGHUP)}

    def restore() -> None:
        for s, h in previous.items():
            signal.signal(s, h)

    return restore


def _left_faulted(event: str, at_ms: int) -> Json:
    """The integrity finding of a heal that failed twice: no later window is
    judged on a system under test that may still be faulted."""
    return {
        "kind": "incident_failed",
        "event": event,
        "at_ms": at_ms,
        "detail": "its heal failed twice: the schedule stopped and the load drained there",
    }


def _undrained(stream_docs: list[Json]) -> list[Json]:
    """A `run` stream's generator that did not end as its drain promises
    (killed at the drain bound, or a bad exit) is a generator failure: its last
    calls are unknown. A load incident's stream fails its heal step instead."""
    return [
        {
            "kind": "generator_failure",
            "stream": d["stream"],
            "detail": f"the generator did not drain cleanly (exit code {d.get('exit_code')})",
        }
        for d in stream_docs
        if d.get("start") == "run" and d.get("drained") is False
    ]


def _dump(p: Path, doc: object) -> None:
    p.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")


@dataclass
class _Live:
    streams: StreamSet
    died: set[str] = field(default_factory=lambda: set())
    loading: bool = False
    event: tuple[PlannedEvent, list[Incident]] | None = None
    load_start_ms: int = 0
    off: OffSchedule | None = None
    fresh: Freshness | None = None
    capture: Captures | None = None
    steps: RateSteps | None = None


class Runner:
    def __init__(
        self,
        content: Content,
        schemas: SchemaSet,
        lab: Lab,
        executor: Executor,
        clock: Clock,
        results: Path,
        say: Callable[[str], None] = _say,
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

    def refusals(self, profile: Profile, capabilities: list[str]) -> list[str]:
        errs = profile_errors(profile, self.content)
        if not errs:
            errs += self._loadgen_refusals(profile)
        for ev in plan(profile, self.content.incidents):
            for name in ev.spec.incidents:
                inc = self.content.incidents[name]
                missing = sorted(set(inc.requires) - set(capabilities))
                if missing:
                    errs.append(f"incident {name!r} requires {missing}, which the lab lacks")
        if len(profile.streams) > self.lab.sipp_ip_slots:
            errs.append(f"{len(profile.streams)} streams exceed the lab's {self.lab.sipp_ip_slots} generator slots")
        if profile.cdr_join and "cdr" not in capabilities:
            errs.append("cdr_join needs the lab capability `cdr` (a CDR consumer shipping observations)")
        return errs

    def _loadgen_refusals(self, profile: Profile) -> list[str]:
        """Each loadgen stream's product image accepts its flags and registers
        its shape (ADR-0049 §5.4)."""
        errs: list[str] = []
        for slot, s in enumerate(profile.streams):
            if s.driver != "loadgen" or slot >= self.lab.sipp_ip_slots:
                continue
            gen = LoadgenStream("check", s, slot, Path("/nonexistent"), self.lab, self.executor)
            err = gen.shape_check()
            if err is not None:
                errs.append(err)
        return errs

    def run(self, profile_name: str) -> Path:
        profile = self.content.profiles.get(profile_name)
        if profile is None:
            raise RunRefused(f"profile {profile_name!r} is not on the search path")
        errs = self.refusals(profile, lab_capabilities(self.lab, self.executor))
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
        streams = StreamSet(run, run_dir, profile, self.content, self.lab, self.executor, self.clock, self.vlogs, started)
        live = _Live(streams)
        # Appended as found, so `hakit probe` reads them while the run runs.
        integrity = JsonlLog(run_dir / "integrity.jsonl")
        steps_log = JsonlLog(run_dir / "steps.jsonl")

        def wait_until(target_ms: int) -> None:
            self._wait(target_ms, journal, live, integrity, steps_log)

        load = StreamLoad(streams, self.clock, wait_until)
        events = EventRunner(run, run_dir, self.lab, self.executor, self.clock, journal, wait_until, load)
        live.off = OffSchedule(
            run_dir, profile, self.content, journal, events, self.clock, lambda: live.load_start_ms, self.say
        )
        self.say(f"hakit: run {run} in {run_dir}")
        mark_active(run_dir)
        restore = _raise_on_termination()
        try:
            if profile.capture is not None:
                live.capture = Captures(run, run_dir, profile.capture, self.lab, self.executor)
                try:
                    live.capture.start()
                except CaptureFailed as e:
                    raise RunRefused(str(e)) from e
            err = streams.start_run_streams()
            if err is not None:
                raise RunRefused(err)
            for s in profile.streams:
                if s.start == "run":
                    self.say(f"hakit: stream {s.name} up ({s.cps:g} cps, shape {s.shape})")
            load_start = self.clock.now_ms()
            live.load_start_ms = load_start
            meta = json.loads((run_dir / "run-meta.json").read_text())
            _dump(run_dir / "run-meta.json", {**meta, "load_started_at_ms": load_start})
            live.fresh = self._freshness(run, profile, started)
            live.steps = RateSteps.of(profile)
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
                if outcome.heal_failed:
                    integrity.add([_left_faulted(ev.event_id, self.clock.now_ms())])
                    self.say(f"hakit: event {ev.event_id} could not be healed: the schedule stops, the load drains")
                    break
            else:
                wait_until(load_start + int(profile.duration_s * 1000))
            live.loading = False
            streams.drain(self.clock.now_ms() + int(profile.drain_s * 1000), wait_until)
            stream_docs = streams.docs()
            integrity.add(_undrained(stream_docs))
            close = journal.next_close_ms()
            while close is not None:
                wait_until(close)
                close = journal.next_close_ms()
        except BaseException:
            in_progress = live.event or live.off.current
            if in_progress is not None:
                ev, incs = in_progress
                events.heal_only(ev, incs)
            streams.remove_all()
            if live.capture is not None:
                live.capture.stop()
            raise
        finally:
            restore()
            clear_active(run_dir)
        streams.remove_all()
        if live.capture is not None:
            live.capture.stop()
        records, bad = self.vlogs.export(run, started - 3_600_000, self.clock.now_ms() + 3_600_000)
        write_jsonl(run_dir / "records.jsonl", records)
        integrity.add({"kind": "generator_failure", "detail": f"unreadable record line: {b}"} for b in bad)
        if profile.cdr_join:
            integrity.add(collect_cdrs(self.vlogs, self.vm, self.lab, self.clock, run, run_dir, started))
        run_counts(self.content.classes, run_dir, self.executor)
        _dump(run_dir / "streams.json", stream_docs)
        write_jsonl(run_dir / "integrity.jsonl", integrity.items)
        meta = json.loads((run_dir / "run-meta.json").read_text())
        meta["ended_at_ms"] = self.clock.now_ms()
        meta["load_started_at_ms"] = live.load_start_ms
        _dump(run_dir / "run-meta.json", meta)
        self.write_verdict(run_dir)
        return run_dir

    def write_verdict(self, run_dir: Path) -> Json:
        """`verdict.json`, `summary.md`, every impact row's full Call-ID list
        (`impact-calls.jsonl`) and the tree that computed them
        (`verdict-tree.json`: a verdict may be recomputed by a later tree)."""
        v, calls = compute_with_calls(run_dir, self.schemas)
        errs = self.schemas.errors("verdict.v1", v)
        if errs:
            raise AssertionError(f"the verdict breaks verdict.v1: {errs[:3]}")
        _dump(run_dir / "verdict.json", v)
        write_jsonl(run_dir / "impact-calls.jsonl", calls)
        tree = self._tree()
        _dump(run_dir / "verdict-tree.json", {**tree, "computed_at_ms": self.clock.now_ms()})
        dirty = f", {tree['dirty_files']} files uncommitted" if tree["dirty_files"] else ""
        (run_dir / "summary.md").write_text(render_summary(v) + f"\nComputed by tree {tree['head']}{dirty}.\n")
        if not self.vm.push(verdict_lines(v, self.clock.now_ms())):
            note = f"the verdict series did not reach VictoriaMetrics ({self.lab.vm_url}): the dashboards miss this verdict"
            self.say(f"hakit: {note}")
            with (run_dir / "summary.md").open("a") as fh:
                fh.write(f"\n> {note}\n")
        return v

    def _tree(self) -> Json:
        """The commit of the tooling's tree and how many files differ from it."""
        head = self.executor.run(["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"], timeout_s=10)
        dirty = self.executor.run(["git", "-C", str(REPO_ROOT), "status", "--porcelain"], timeout_s=30)
        return {"head": head.stdout.strip(), "dirty_files": len(dirty.stdout.splitlines())}

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
        meta: Json = {
            "run": run,
            "profile": profile.name,
            "started_at_ms": started,
            "ended_at_ms": started,
            "tree": self._tree(),
            "search_path": [str(p) for p in self.content.path],
            "host": facts,
            "sut_calls_at_start": held,
            "classes": sorted(self.content.classes),
        }
        _dump(run_dir / "run-meta.json", meta)

    def _freshness(self, run: str, profile: Profile, started: int) -> Freshness:
        streams = [(s.name, s.cps, max(s.setup_deadline_s, s.ring_s + s.hold_s)) for s in profile.streams if s.start == "run"]
        return Freshness(run, self.vm, self.vlogs, self.clock, streams, self.lab.live_series, started)

    def _wait(self, target_ms: int, journal: EventJournal, live: _Live, integrity: JsonlLog, steps_log: JsonlLog) -> None:
        while True:
            journal.tick()
            for event, opened, _closed in journal.take_closed():
                if live.capture is not None:
                    live.capture.keep(event, opened)
            if live.loading:
                if live.steps is not None:
                    lines, refused = live.steps.apply_due(live.load_start_ms, self.clock.now_ms(), live.streams.set_rate)
                    steps_log.add(lines)
                    integrity.add(refused)
                for s in live.streams.live():
                    key = f"{s.stream.name}{s.call_id_tag}"
                    if key not in live.died and not s.running():
                        at = self.clock.now_ms()
                        live.died.add(key)
                        integrity.add(
                            [{"kind": "stream_died", "stream": s.stream.name, "at_ms": at, "detail": "generator exited"}]
                        )
                        if live.off is not None:
                            live.off.detected("stream_died")
                if live.fresh is not None:
                    integrity.add(live.fresh.tick())
                if live.off is not None:
                    live.off.poll()
            now = self.clock.now_ms()
            if now >= target_ms:
                return
            step = min(target_ms - now, KEEPALIVE_MS)
            wake = [journal.next_close_ms()]
            if live.loading and live.steps is not None:
                wake.append(live.steps.next_ms(live.load_start_ms))
            for at in wake:
                if at is not None and at > now:
                    step = min(step, at - now)
            self.clock.sleep(step / 1000)

