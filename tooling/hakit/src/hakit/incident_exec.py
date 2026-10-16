"""One event through the incident contract (ADR-0049 §2.2 to §2.6, §2.8).

Phases in order: `inject`, `injected`, then after the hold `heal`, `healed`,
then `recovered` once `probe` exits 0 (at once without a probe). A step that
fails or overruns its timeout records `failed`. Incidents inject in the listed
order and heal in reverse; heal runs for every incident whose inject was
attempted, whatever happened after it. A fault's inject and heal are its
executables; a load incident's are the start and the stop of its stream,
which the runner owns (`LoadSteps`).
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from .clock import Clock
from .events import EventJournal
from .executor import Executor
from .jsonval import Json, as_obj
from .lab import Lab
from .model import Incident, Scalar
from .paths import FAULT_LIB
from .schedule import PlannedEvent

PROBE_NOT_YET = 75

type Waiter = Callable[[int], None]


class LoadSteps(Protocol):
    """The inject and heal of a load incident: start and stop the profile stream
    its event names. Each returns whether the step succeeded and its facts;
    a stop's `exited_at_ms` fact is the instant its generator exited, which
    `healed` takes (ADR-0049 §2.8)."""

    def start(self, inc: Incident, ev: PlannedEvent) -> tuple[bool, Json]: ...

    def stop(self, inc: Incident, ev: PlannedEvent, interrupted: bool) -> tuple[bool, Json]: ...


@dataclass
class EventOutcome:
    opened_at_ms: int
    failed: bool
    last_recovered_ms: int | None
    # A heal failed twice: the system under test may still be faulted.
    heal_failed: bool = False


def _params(inc: Incident, ev: PlannedEvent) -> dict[str, Scalar]:
    raw = ev.spec.params.get(inc.name, {}) if len(ev.spec.incidents) > 1 else ev.spec.params
    given = as_obj(raw)
    out: dict[str, Scalar] = {k: p.default for k, p in inc.params.items() if p.default is not None}
    for k, v in given.items():
        if isinstance(v, (str, int, float, bool)):
            out[k] = v
    return out


def _target(inc: Incident, ev: PlannedEvent, lab: Lab) -> tuple[str, str, str]:
    raw = ev.spec.target.get(inc.name, {}) if len(ev.spec.incidents) > 1 else ev.spec.target
    over = as_obj(raw)
    base = inc.target
    selector = str(over.get("selector", base.selector if base else ""))
    namespace = str(over.get("namespace", (base.namespace if base else None) or lab.namespace))
    pick = str(over.get("pick", base.pick if base else "one"))
    return selector, namespace, pick


def fmt_param(v: Scalar) -> str:
    """A parameter as its `HAKIT_PARAM_*` variable spells it."""
    if isinstance(v, bool):
        return "true" if v else "false"
    return f"{v:g}" if isinstance(v, float) else str(v)


class EventRunner:
    def __init__(
        self,
        run: str,
        run_dir: Path,
        lab: Lab,
        executor: Executor,
        clock: Clock,
        journal: EventJournal,
        wait_until: Waiter,
        load: LoadSteps,
    ) -> None:
        self.run = run
        self.run_dir = run_dir
        self.lab = lab
        self.executor = executor
        self.clock = clock
        self.journal = journal
        self.wait_until = wait_until
        self.load = load

    def env(self, inc: Incident, ev: PlannedEvent, event_dir: Path) -> dict[str, str]:
        selector, namespace, pick = _target(inc, ev, self.lab)
        env = {
            "HAKIT_RUN": self.run,
            "HAKIT_RUN_DIR": str(self.run_dir),
            "HAKIT_EVENT": ev.event_id,
            "HAKIT_EVENT_DIR": str(event_dir),
            "HAKIT_INCIDENT": inc.name,
            "HAKIT_TARGET_SELECTOR": selector,
            "HAKIT_TARGET_NAMESPACE": namespace,
            "HAKIT_TARGET_PICK": pick,
            "HAKIT_KUBE_CONTEXT": self.lab.kube_context,
            "HAKIT_DURATION_S": fmt_param(ev.duration_s),
            "HAKIT_LIB": str(FAULT_LIB),
            "HAKIT_VM_IMPORT": self.lab.vm_import_url,
        }
        for k, v in _params(inc, ev).items():
            env[f"HAKIT_PARAM_{k.upper()}"] = fmt_param(v)
        return env

    def step(self, inc: Incident, ev: PlannedEvent, step: str, timeout_s: float) -> tuple[int, Json | None]:
        """Run one executable; its exit code (or -1 on a broken output contract) and its facts."""
        exe = inc.executable(step)
        if exe is None:
            return 0, None
        event_dir = self.run_dir / "events" / ev.event_id / inc.name
        event_dir.mkdir(parents=True, exist_ok=True)
        res = self.executor.run([str(exe)], env=self.env(inc, ev, event_dir), cwd=event_dir, timeout_s=timeout_s)
        with (event_dir / f"{step}.stderr").open("a") as fh:
            fh.write(res.stderr)
            if res.timed_out:
                fh.write(f"\nhakit: {step} overran its {timeout_s:g} s timeout; process group killed\n")
        out = res.stdout.strip()
        if not out:
            return res.rc, None
        try:
            return res.rc, as_obj(json.loads(out))
        except ValueError:
            with (event_dir / f"{step}.stderr").open("a") as fh:
                fh.write(f"\nhakit: stdout is not one JSON object: {out[:200]!r}\n")
            return -1, None

    def _simple(
        self,
        inc: Incident,
        ev: PlannedEvent,
        step: str,
        done: str,
        timeout_s: float,
        interrupted: bool = False,
        quiet: bool = False,
    ) -> bool:
        """Run one step and journal its outcome phase; with `quiet` a failure
        journals no `failed` (the caller retries)."""
        if inc.kind == "load":
            ok, load_facts = (
                self.load.start(inc, ev) if step == "inject" else self.load.stop(inc, ev, interrupted)
            )
            self.journal.facts(ev.event_id, inc.name, step, load_facts)
            exited = load_facts.get("exited_at_ms")
            at = int(exited) if ok and isinstance(exited, (int, float)) else None
            if ok or not quiet:
                self.journal.phase(ev.event_id, inc.name, done if ok else "failed", at)
            return ok
        rc, facts = self.step(inc, ev, step, timeout_s)
        if facts is not None:
            self.journal.facts(ev.event_id, inc.name, step, facts)
        if rc == 0 or not quiet:
            self.journal.phase(ev.event_id, inc.name, done if rc == 0 else "failed")
        return rc == 0

    def _heal(self, inc: Incident, ev: PlannedEvent, interrupted: bool = False) -> bool:
        """Heal once more after a failed fault heal; the retry is in the
        incident's facts (`hakit` / `heal_retry`), and only a second failure is
        `failed`. A load stop is not retried: its generator is gone either way,
        and a stop that failed lost the calls it was draining."""
        if inc.kind == "load":
            return self._simple(inc, ev, "heal", "healed", inc.timeouts.heal_s, interrupted)
        if self._simple(inc, ev, "heal", "healed", inc.timeouts.heal_s, interrupted, quiet=True):
            return True
        self.journal.facts(ev.event_id, inc.name, "hakit", {"heal_retry": {"at_ms": self.clock.now_ms()}})
        return self._simple(inc, ev, "heal", "healed", inc.timeouts.heal_s, interrupted)

    def _probe(self, inc: Incident, ev: PlannedEvent) -> bool:
        if inc.executable("probe") is None:
            self.journal.phase(ev.event_id, inc.name, "recovered")
            return True
        t = inc.timeouts
        deadline = self.clock.now_ms() + int(t.probe_s * 1000)
        while True:
            rc, facts = self.step(inc, ev, "probe", t.probe_interval_s + 30)
            if rc == 0:
                if facts is not None:
                    self.journal.facts(ev.event_id, inc.name, "probe", facts)
                self.journal.phase(ev.event_id, inc.name, "recovered")
                return True
            if rc != PROBE_NOT_YET or self.clock.now_ms() >= deadline:
                self.journal.phase(ev.event_id, inc.name, "failed")
                return False
            self.wait_until(min(deadline, self.clock.now_ms() + int(t.probe_interval_s * 1000)))

    def run_event(self, ev: PlannedEvent, incs: list[Incident], source: str) -> EventOutcome:
        max_s = max(i.max_s(ev.duration_s) for i in incs)
        self.journal.begin(ev.event_id, source, ev.spec.incidents, ev.spec.budget, ev.settle_s, max_s)
        opened = self.clock.now_ms()
        attempted: list[Incident] = []
        failed = False
        for inc in incs:
            self.journal.phase(ev.event_id, inc.name, "inject")
            attempted.append(inc)
            if not self._simple(inc, ev, "inject", "injected", inc.timeouts.inject_s):
                failed = True
                break
        if not failed:
            self.wait_until(self.clock.now_ms() + int(ev.duration_s * 1000))
        heal_failed = False
        for inc in reversed(attempted):
            self.journal.phase(ev.event_id, inc.name, "heal")
            if not self._heal(inc, ev):
                failed = True
                heal_failed = heal_failed or inc.kind == "fault"
        last: int | None = None
        if not failed:
            for inc in incs:
                if not self._probe(inc, ev):
                    failed = True
                    break
            last = self.clock.now_ms()
        settle_ms = int(ev.settle_s * 1000)
        close = opened + int(max_s * 1000) + settle_ms if failed or last is None else last + settle_ms
        self.journal.close_at(ev.event_id, close)
        return EventOutcome(opened_at_ms=opened, failed=failed, last_recovered_ms=last, heal_failed=heal_failed)

    def heal_only(self, ev: PlannedEvent, incs: list[Incident]) -> list[str]:
        """Heal every incident of an event left open (an interrupted run, `hakit
        cleanup`), journaling the phases; the incidents whose heal failed."""
        failed: list[str] = []
        for inc in reversed(incs):
            self.journal.phase(ev.event_id, inc.name, "heal")
            if not self._heal(inc, ev, interrupted=True):
                failed.append(inc.name)
        return failed
