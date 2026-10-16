"""Events off the schedule: `hakit fire` requests and detected events (ADR-0049 §3.9).

`hakit fire` leaves `fire/<id>.request` (`{"incident": name}`) in the running
run's directory and waits for `fire/<id>.answer` (`{"accepted", "reason",
"event"}`). The runner takes requests between two waits: one is refused without
a `defaults` entry for the incident, for a load incident, while a window is
open, or when its window (`expected_s` + `settle_s`) would reach a scheduled
inject; otherwise its event `m<seq>-<incident>` runs with the entry's budget,
parameters and target. A detected event `d<seq>-<name>` heals nothing: its
window opens at the detection and closes `settle_s` later, under the `defaults`
entry of its name or a budget that allows nothing.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable
from pathlib import Path

from . import jsonval as jv
from .clock import Clock
from .content import Content
from .events import EventJournal
from .incident_exec import EventRunner
from .jsonval import Json, as_obj
from .model import Incident
from .profile import EventSpec, Profile
from .schedule import PlannedEvent, plan, plan_event

ACTIVE = "active"
FIRE_DIR = "fire"


def defaults_spec(profile: Profile, incident: str) -> EventSpec | None:
    entry = profile.defaults.get(incident)
    if entry is None:
        return None
    return EventSpec(
        incidents=[incident],
        budget=jv.string(entry, "budget"),
        at_s=None,
        stream=jv.opt_string(entry, "stream"),
        params=jv.obj(entry, "params"),
        target=jv.obj(entry, "target"),
        duration_s=None,
        expected_s=None,
        settle_s=None,
    )


def mark_active(run_dir: Path) -> None:
    (run_dir / ACTIVE).write_text(f"{os.getpid()}\n")


def clear_active(run_dir: Path) -> None:
    (run_dir / ACTIVE).unlink(missing_ok=True)


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def active_runs(results: Path) -> list[Path]:
    """The run directories of `results` whose runner is still running."""
    if not results.is_dir():
        return []
    out: list[Path] = []
    for d in sorted(p for p in results.iterdir() if (p / ACTIVE).is_file()):
        try:
            pid = int((d / ACTIVE).read_text().strip())
        except ValueError:
            continue
        if pid_alive(pid):
            out.append(d)
    return out


def request_fire(run_dir: Path, incident: str, clock: Clock, timeout_s: float, nonce: str) -> Json:
    """Leave a fire request in `run_dir` and wait for the runner's answer; a
    refusal when none came within `timeout_s` (the request is withdrawn)."""
    fire = run_dir / FIRE_DIR
    fire.mkdir(exist_ok=True)
    request = fire / f"{nonce}-{incident}.request"
    answer = request.with_suffix(".answer")
    request.write_text(json.dumps({"incident": incident}) + "\n")
    deadline = clock.now_ms() + int(timeout_s * 1000)
    while clock.now_ms() < deadline:
        if answer.is_file():
            return as_obj(json.loads(answer.read_text()))
        clock.sleep(0.5)
    request.unlink(missing_ok=True)
    return {"accepted": False, "reason": f"the runner did not answer within {timeout_s:g} s", "event": ""}


class OffSchedule:
    def __init__(
        self,
        run_dir: Path,
        profile: Profile,
        content: Content,
        journal: EventJournal,
        events: EventRunner,
        clock: Clock,
        load_start_ms: Callable[[], int],
        say: Callable[[str], None],
    ) -> None:
        self.run_dir = run_dir
        self.profile = profile
        self.content = content
        self.journal = journal
        self.events = events
        self.clock = clock
        self.load_start_ms = load_start_ms
        self.say = say
        self.manual = 0
        self.detected_seq = 0
        # The manual event in progress, healed by the runner on an interruption.
        self.current: tuple[PlannedEvent, list[Incident]] | None = None

    def detected(self, name: str) -> str:
        """Open the window of a detected event; its id."""
        self.detected_seq += 1
        event = f"d{self.detected_seq:03d}-{name}"
        entry = self.profile.defaults.get(name)
        budget = jv.string(entry, "budget") if entry is not None else ""
        settle_ms = int(self.profile.settle_s * 1000)
        self.journal.begin(event, "detected", [name], budget, self.profile.settle_s, 0)
        at = self.journal.phase(event, name, "inject")
        self.journal.phase(event, name, "recovered")
        self.journal.close_at(event, at + settle_ms)
        return event

    def _answer(self, request: Path, accepted: bool, reason: str, event: str = "") -> None:
        doc: Json = {"accepted": accepted, "reason": reason, "event": event}
        request.with_suffix(".answer").write_text(json.dumps(doc) + "\n")
        request.unlink(missing_ok=True)

    def _refusal(self, incident: str) -> str | None:
        inc = self.content.incidents.get(incident)
        if inc is None:
            return f"incident {incident!r} is not on the search path"
        if incident not in self.profile.defaults:
            return f"profile {self.profile.name!r} has no defaults entry for {incident!r}"
        if inc.kind != "fault":
            return f"incident {incident!r} is a load incident; fire runs fault incidents"
        if self.journal.open:
            return f"a window is open ({', '.join(sorted(self.journal.open))})"
        return None

    def _scheduled_clash(self, ev: PlannedEvent) -> str | None:
        """Why the manual event's window (its `expected_s` + `settle_s`) would reach
        a scheduled inject still to come, or None."""
        for sched in plan(self.profile, self.content.incidents):
            if ev.at_s <= sched.at_s < ev.window_end_s:
                return (
                    f"its window (expected {ev.expected_s:g} s + settle {ev.settle_s:g} s) would reach event "
                    f"{sched.event_id} at +{sched.at_s:g} s"
                )
        return None

    def poll(self) -> None:
        """Take every pending `hakit fire` request, oldest first."""
        fire = self.run_dir / FIRE_DIR
        if not fire.is_dir():
            return
        for request in sorted(fire.glob("*.request")):
            if not request.is_file():
                continue  # answered by a poll nested in an event this loop ran
            try:
                incident = jv.string(as_obj(json.loads(request.read_text())), "incident")
            except ValueError as e:
                self._answer(request, False, f"unreadable request: {e}")
                continue
            why = self._refusal(incident)
            spec = defaults_spec(self.profile, incident)
            if why is not None or spec is None:
                self._answer(request, False, why or "no defaults entry")
                continue
            at_s = (self.clock.now_ms() - self.load_start_ms()) / 1000
            ev = plan_event(self.manual + 1, at_s, spec, self.profile, self.content.incidents, prefix="m")
            clash = self._scheduled_clash(ev)
            if clash is not None:
                self._answer(request, False, clash)
                continue
            self.manual += 1
            self._answer(request, True, "fired", ev.event_id)
            self.say(f"hakit: manual event {ev.event_id} (budget {spec.budget})")
            incs = [self.content.incidents[incident]]
            self.current = (ev, incs)
            self.events.run_event(ev, incs, "manual")
            self.current = None
