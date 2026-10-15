"""What a schema cannot express about a profile, checked before anything runs.

References resolve (incidents, budgets, catalogs, shapes, scenarios), event
parameters are set and of their declared type, the timing bounds of ADR-0049
§3.7 hold, and the planned windows do not overlap.
"""

from __future__ import annotations

from .content import Content
from .jsonval import as_obj
from .model import Incident
from .profile import EventSpec, Profile
from .schedule import overlaps, plan

_PY_TYPES: dict[str, tuple[type, ...]] = {
    "string": (str,),
    "integer": (int,),
    "number": (int, float),
    "boolean": (bool,),
}


def _typed(value: object, declared: str) -> bool:
    if isinstance(value, bool) and declared != "boolean":
        return False
    return isinstance(value, _PY_TYPES[declared])


def _event_errors(where: str, ev: EventSpec, profile: Profile, content: Content) -> list[str]:
    errs: list[str] = []
    if ev.budget not in content.budgets:
        errs.append(f"{where}: budget {ev.budget!r} is not on the search path")
    incs: list[Incident] = []
    for name in ev.incidents:
        inc = content.incidents.get(name)
        if inc is None:
            errs.append(f"{where}: incident {name!r} is not on the search path")
        else:
            incs.append(inc)
    loads = [i for i in incs if i.kind == "load"]
    if len(loads) > 1:
        errs.append(f"{where}: at most one load incident per event")
    if loads:
        s = profile.stream(ev.stream or "")
        if s is None or s.start != "incident":
            errs.append(f"{where}: load incident {loads[0].name!r} needs `stream` naming a `start: incident` stream")
    combined = len(ev.incidents) > 1
    for inc in incs:
        params = as_obj(ev.params.get(inc.name, {})) if combined else ev.params
        if inc.kind == "load":
            continue
        for k, v in params.items():
            decl = inc.params.get(k)
            if decl is None:
                errs.append(f"{where}: incident {inc.name!r} has no parameter {k!r}")
            elif not _typed(v, decl.type):
                errs.append(f"{where}: parameter {k!r} of {inc.name!r} must be of type {decl.type}")
        for k, decl in inc.params.items():
            if decl.default is None and k not in params:
                errs.append(f"{where}: parameter {k!r} of {inc.name!r} has no default and is not set")
    return errs


def profile_errors(profile: Profile, content: Content) -> list[str]:
    where = f"profile {profile.name!r}"
    errs: list[str] = []
    for cat in profile.catalogs:
        if cat not in content.catalogs:
            errs.append(f"{where}: catalog {cat!r} is not on the search path")
    names = [s.name for s in profile.streams]
    if len(set(names)) != len(names):
        errs.append(f"{where}: stream names repeat")
    for s in profile.streams:
        shape = content.shapes.get(s.shape)
        if shape is None or shape.catalog not in profile.catalogs:
            errs.append(f"{where}: stream {s.name!r}: shape {s.shape!r} is in none of the profile's catalogs")
        elif not shape.load_safe:
            errs.append(f"{where}: stream {s.name!r}: shape {s.shape!r} is not load_safe")
        if s.driver == "sipp" and (s.scenario or "") not in content.scenarios:
            errs.append(f"{where}: stream {s.name!r}: scenario {s.scenario!r} is not on the search path")
    deadline = max((s.setup_deadline_s for s in profile.streams), default=0.0)
    teardown = max((max(s.setup_deadline_s, s.ring_s + s.hold_s) for s in profile.streams), default=0.0)
    if profile.drain_s < teardown:
        errs.append(f"{where}: drain_s {profile.drain_s:g} is below the longest stream teardown {teardown:g}")
    sched = profile.schedule
    events = [] if sched is None else sched.cycle + sched.at
    for i, ev in enumerate(events):
        ew = f"{where} event {i + 1} ({ev.label})"
        errs.extend(_event_errors(ew, ev, profile, content))
        settle = ev.settle_s if ev.settle_s is not None else profile.settle_s
        if settle < deadline:
            errs.append(f"{ew}: settle_s {settle:g} is below the longest setup_deadline_s {deadline:g}")
    if profile.settle_s < deadline:
        errs.append(f"{where}: settle_s {profile.settle_s:g} is below the longest setup_deadline_s {deadline:g}")
    for name, entry in profile.defaults.items():
        if name not in content.incidents:
            errs.append(f"{where}: defaults name incident {name!r}, which is not on the search path")
        b = entry.get("budget")
        if isinstance(b, str) and b not in content.budgets:
            errs.append(f"{where}: defaults.{name}: budget {b!r} is not on the search path")
    if not errs:
        errs.extend(f"{where}: {e}" for e in overlaps(plan(profile, content.incidents)))
    return errs


def all_profile_errors(content: Content) -> list[str]:
    errs: list[str] = []
    for p in content.profiles.values():
        errs.extend(profile_errors(p, content))
    return errs
