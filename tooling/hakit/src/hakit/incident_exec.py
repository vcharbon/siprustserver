"""One fault event through the incident contract (ADR-0049 §2.2 to §2.6).

Phases in order: `inject`, `injected`, then after the hold `heal`, `healed`,
then `recovered` once `probe` exits 0 (at once without a probe). A step that
fails or overruns its timeout records `failed`. Incidents inject in the listed
order and heal in reverse; heal runs for every incident whose inject was
attempted, whatever happened after it.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

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


@dataclass
class EventOutcome:
    opened_at_ms: int
    failed: bool
    last_recovered_ms: int | None


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


def _fmt(v: Scalar) -> str:
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
    ) -> None:
        self.run = run
        self.run_dir = run_dir
        self.lab = lab
        self.executor = executor
        self.clock = clock
        self.journal = journal
        self.wait_until = wait_until

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
            "HAKIT_DURATION_S": _fmt(ev.duration_s),
            "HAKIT_LIB": str(FAULT_LIB),
            "HAKIT_VM_IMPORT": self.lab.vm_import_url,
        }
        for k, v in _params(inc, ev).items():
            env[f"HAKIT_PARAM_{k.upper()}"] = _fmt(v)
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

    def _simple(self, inc: Incident, ev: PlannedEvent, step: str, done: str, timeout_s: float) -> bool:
        rc, facts = self.step(inc, ev, step, timeout_s)
        if facts is not None:
            self.journal.facts(ev.event_id, inc.name, step, facts)
        self.journal.phase(ev.event_id, inc.name, done if rc == 0 else "failed")
        return rc == 0

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
        for inc in reversed(attempted):
            self.journal.phase(ev.event_id, inc.name, "heal")
            if not self._simple(inc, ev, "heal", "healed", inc.timeouts.heal_s):
                failed = True
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
        return EventOutcome(opened_at_ms=opened, failed=failed, last_recovered_ms=last)

    def heal_only(self, ev: PlannedEvent, incs: list[Incident]) -> list[str]:
        """Heal every incident of an event left open (`hakit cleanup`); the steps that failed."""
        failed: list[str] = []
        for inc in reversed(incs):
            rc, _ = self.step(inc, ev, "heal", inc.timeouts.heal_s)
            if rc != 0:
                failed.append(inc.name)
        return failed
