"""`hakit cleanup`: heal every event a run left open, and remove the generators
of every run whose runner is gone.

A run interrupted hard (its runner killed, the box rebooted) leaves its open
events faulted and its generators placing calls. Cleanup heals the events of
the named run directories, or of every one under the results root, and
removes each generator labelled with a run whose runner no longer runs,
whether or not an event of it was open: docker containers and pods alike.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from pathlib import Path

from .clock import Clock
from .content import Content
from .events import EventJournal
from .executor import Executor
from .incident_exec import EventRunner
from .jsonval import as_obj
from .lab import Lab
from .offschedule import active_runs, defaults_spec
from .preflight import open_events
from .profile import Profile, parse_profile
from .schedule import PlannedEvent, plan, plan_event
from .streams import RemoveLoad
from .vm import VictoriaMetrics

RUN_LABEL = "hakit-run"


def manual_events(profile: Profile, content: Content, ids: Sequence[str]) -> list[PlannedEvent]:
    """The manual events among `ids` (`m<seq>-<incident>`), planned from the profile's defaults."""
    out: list[PlannedEvent] = []
    for event in ids:
        seq, _, incident = event.removeprefix("m").partition("-")
        spec = defaults_spec(profile, incident) if event.startswith("m") and seq.isdigit() else None
        if spec is not None and incident in content.incidents:
            out.append(plan_event(int(seq), 0, spec, profile, content.incidents, prefix="m"))
    return out


def heal_open(content: Content, lab: Lab, executor: Executor, clock: Clock, d: Path) -> int:
    """Heal the open events of one run directory; the number of heals that failed."""
    ids = open_events(d)
    if not ids:
        return 0
    run = d.name
    profile = parse_profile(as_obj(json.loads((d / "profile.json").read_text())))
    journal = EventJournal(run, d, clock, VictoriaMetrics(executor, lab.vm_url))
    stepper = EventRunner(run, d, lab, executor, clock, journal, lambda _t: None, RemoveLoad(executor, run, lab))
    failed = 0
    for ev in plan(profile, content.incidents) + manual_events(profile, content, ids):
        if ev.event_id in ids:
            bad = stepper.heal_only(ev, [content.incidents[n] for n in ev.spec.incidents])
            print(f"hakit: {run}/{ev.event_id} healed" + (f", failed: {bad}" if bad else ""))
            failed += len(bad)
    return failed


def generator_runs(executor: Executor, lab: Lab) -> set[str]:
    """The runs that still have a generator, as docker containers or pods."""
    runs: set[str] = set()
    ps = executor.run(["docker", "ps", "-a", "--filter", f"label={RUN_LABEL}", "--format", f'{{{{.Label "{RUN_LABEL}"}}}}'], timeout_s=30)
    runs |= {r for r in ps.stdout.split() if r}
    pods = executor.run(
        ["kubectl", "--context", lab.kube_context, "-n", lab.namespace, "get", "pods", "-l", RUN_LABEL]
        + ["-o", f"jsonpath={{range .items[*]}}{{.metadata.labels.{RUN_LABEL}}}{{\"\\n\"}}{{end}}"],
        timeout_s=30,
    )
    runs |= {r for r in pods.stdout.split() if r}
    return runs


def remove_generators(executor: Executor, lab: Lab, run: str) -> None:
    ps = executor.run(["docker", "ps", "-aq", "--filter", f"label={RUN_LABEL}={run}"], timeout_s=30)
    ids = ps.stdout.split()
    if ids:
        executor.run(["docker", "rm", "-f", *ids], timeout_s=60)
    executor.run(
        ["kubectl", "--context", lab.kube_context, "-n", lab.namespace, "delete", "pods", "-l", f"{RUN_LABEL}={run}"]
        + ["--grace-period=0", "--force", "--ignore-not-found"],
        timeout_s=120,
    )


def cleanup(
    content: Content, lab: Lab, executor: Executor, clock: Clock, root: Path, run_dirs: Sequence[Path], dry_run: bool
) -> int:
    dirs = list(run_dirs) or ([d for d in sorted(root.iterdir()) if d.is_dir()] if root.is_dir() else [])
    live = {d.name for d in active_runs(root)} | {d.name for d in run_dirs if d in active_runs(d.parent)}
    failed = 0
    for d in dirs:
        if d.name in live or not open_events(d):
            continue
        bad = heal_open(content, lab, executor, clock, d)
        failed += bad
        if not bad and not dry_run:
            (d / "cleaned").write_text("")
    named = {d.name for d in run_dirs}
    for run in sorted(generator_runs(executor, lab) - live):
        if named and run not in named:
            continue
        remove_generators(executor, lab, run)
        print(f"hakit: {run}: generators removed")
    return 1 if failed else 0
