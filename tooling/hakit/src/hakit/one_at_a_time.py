"""Pods a profile faults one at a time (`one_at_a_time`): a redundant
backend whose replicas the run never takes down together.

Each entry names a label selector, compared as a set of requirements. Every
scheduled event aimed at it (its incident's target selector, or the event's
own) fires one incident on it, which picks one pod, and the next
such event fires at least `gap_s` after the previous one's recovery bound
(`at_s + expected_s`): the client's time to spread its connections again
before another replica is faulted. Before the load, at least `min_ready` pods
of the selector are ready, so another one serves at every instant.
"""

from __future__ import annotations

import json

from . import jsonval as jv
from .executor import Executor
from .lab import Lab
from .model import Incident
from .profile import OneAtATime, Profile
from .schedule import PlannedEvent, target_of


def normalised(selector: str) -> tuple[str, ...]:
    """A label selector as a set of requirements: `a=b, c=d` and `c=d,a=b`
    are one selector."""
    return tuple(sorted(p.strip().replace(" ", "") for p in selector.split(",") if p.strip()))


def _aimed(spec: OneAtATime, ev: PlannedEvent, incidents: dict[str, Incident]) -> list[tuple[str, str]]:
    """The incidents of `ev` aimed at `spec`'s selector, each with its pick."""
    out: list[tuple[str, str]] = []
    want = normalised(spec.selector)
    for name in ev.spec.incidents:
        inc = incidents.get(name)
        if inc is None:
            continue
        selector, _, pick = target_of(inc, ev.spec)
        if normalised(selector) == want:
            out.append((name, pick))
    return out


def one_at_a_time_errors(where: str, profile: Profile, events: list[PlannedEvent], incidents: dict[str, Incident]) -> list[str]:
    """The planned events that could fault two pods of a selector at once."""
    errs: list[str] = []
    for spec in profile.one_at_a_time:
        w = f"{where}: one_at_a_time {spec.selector!r}"
        if spec.min_ready < 2:
            errs.append(f"{w}: min_ready {spec.min_ready} leaves no pod to serve while one is faulted")
        previous: PlannedEvent | None = None
        for ev in events:
            aimed = _aimed(spec, ev, incidents)
            if not aimed:
                continue
            for name, pick in aimed:
                if pick != "one":
                    errs.append(f"{w}: event {ev.event_id} fires {name} with pick {pick!r}, not 'one'")
            if len(aimed) > 1:
                names = ", ".join(n for n, _ in aimed)
                errs.append(f"{w}: event {ev.event_id} fires {names} together, each on its own pod")
            if previous is not None:
                ready_at = previous.at_s + previous.expected_s + spec.gap_s
                if ev.at_s < ready_at:
                    errs.append(
                        f"{w}: event {ev.event_id} at {ev.at_s:g}s fires before {previous.event_id}'s pod is "
                        f"back in use ({previous.at_s:g}s + expected {previous.expected_s:g}s + gap {spec.gap_s:g}s "
                        f"= {ready_at:g}s)"
                    )
            previous = ev
    return errs


def ready_errors(profile: Profile, lab: Lab, executor: Executor) -> list[str]:
    """Each selector of `profile.one_at_a_time` with fewer ready pods than
    its `min_ready`, read from the API server now."""
    errs: list[str] = []
    for spec in profile.one_at_a_time:
        res = executor.run(
            ["kubectl", "--context", lab.kube_context, "-n", lab.namespace, "get", "pods", "-l", spec.selector, "-o", "json"],
            timeout_s=30,
        )
        if res.rc != 0:
            errs.append(f"one_at_a_time {spec.selector!r}: the pods cannot be read: {res.stderr.strip()[:200]}")
            continue
        ready = ready_pods(res.stdout)
        if ready is None:
            errs.append(f"one_at_a_time {spec.selector!r}: the API server's answer is no pod list")
        elif ready < spec.min_ready:
            errs.append(f"one_at_a_time {spec.selector!r}: {ready} pods ready, under min_ready {spec.min_ready}")
    return errs


def ready_pods(pod_list: str) -> int | None:
    """The pods of a `kubectl get pods -o json` answer that are ready and not
    being deleted; `None` when it is no pod list."""
    try:
        doc = jv.as_obj(json.loads(pod_list))
        if not isinstance(doc.get("items"), list):
            return None
        n = 0
        for pod in jv.objs(doc, "items"):
            if jv.opt_string(jv.obj(pod, "metadata"), "deletionTimestamp") is not None:
                continue
            conditions = jv.objs(jv.obj(pod, "status"), "conditions")
            if any(jv.opt_string(c, "type") == "Ready" and jv.opt_string(c, "status") == "True" for c in conditions):
                n += 1
        return n
    except (ValueError, jv.ShapeError):
        return None
