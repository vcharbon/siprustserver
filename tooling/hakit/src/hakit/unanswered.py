"""Every call that did not end answered, by cause and by where, per stream and
for priority calls against the others: a reading of the records, never a
judgement (no budget, no finding).

A call falls in exactly one cause, tried in this order: `expected` (an ending
of its shape other than answered), `overload` (a 503 with Retry-After),
`dependency_fault` (a 503 without Retry-After whose setup, start to setup end,
overlaps a window), `setup_timeout`, `cut` (cut, stranded, or failed after its
answer), `other` (any other ending or code). Its place is the incident kinds of
the first window its setup overlaps (a cut: the window holding its cut
instant), or `outside`. Its detail is its ending and final code, with its early
media when an expected ending of that code names one. A priority stream places
calls whose shape's request carries `Resource-Priority` (RFC 4412); the groups
hold `run` streams only.
"""

from __future__ import annotations

from bisect import bisect_right
from collections import Counter
from collections.abc import Iterable
from dataclasses import dataclass, field

from . import jsonval as jv
from .classify import caller_expected
from .jsonval import Json
from .model import Shape
from .profile import Profile
from .windows import Window

CAUSES = ("expected", "overload", "dependency_fault", "setup_timeout", "cut", "other")
OUTSIDE = "outside"
PRIORITY_HEADER = "resource-priority"
OVERLOAD_CODE = 503
PRIORITY = "priority"
OTHERS = "others"


def is_priority(shape: Shape) -> bool:
    """The shape's request carries `Resource-Priority` (RFC 4412)."""
    return any(name.lower() == PRIORITY_HEADER for name, _ in shape.headers)


def kind_of(w: Window) -> str:
    """The window's incident kind: its incidents' names, joined by `+`."""
    return "+".join(w.incidents)


class Places:
    """Which window a call's instant or setup interval falls in."""

    def __init__(self, windows: list[Window]) -> None:
        self.windows = sorted(windows, key=lambda w: w.opened_at_ms)
        self.opens = [w.opened_at_ms for w in self.windows]
        self.reach: list[int] = []
        for w in self.windows:
            self.reach.append(max(self.reach[-1], w.closed_at_ms) if self.reach else w.closed_at_ms)

    def overlapping(self, from_ms: float, to_ms: float) -> Window | None:
        """The earliest-opened window that overlaps [from_ms, to_ms]."""
        found: Window | None = None
        i = bisect_right(self.opens, to_ms) - 1
        while i >= 0 and self.reach[i] >= from_ms:
            if self.windows[i].closed_at_ms >= from_ms:
                found = self.windows[i]
            i -= 1
        return found

    def where(self, from_ms: float, to_ms: float) -> str:
        w = self.overlapping(from_ms, to_ms)
        return OUTSIDE if w is None else kind_of(w)


@dataclass(frozen=True)
class Cause:
    cause: str
    detail: str
    where: str


def cause_of(r: Json, shape: Shape, places: Places) -> Cause | None:
    """The cause and place of a call that did not end answered; None for an answered call its shape expects."""
    ending = jv.string(r, "ending")
    code = jv.opt_int(r, "final_code")
    started = jv.number(r, "started_at_ms")
    ended = jv.number(r, "ended_at_ms")
    setup_end = jv.opt_number(r, "setup_ended_at_ms")
    setup_where = places.where(started, setup_end if setup_end is not None else ended)
    detail = ending if code is None else f"{ending} {code}"
    if ending == "answered" and shape.expects_answer:
        return None
    if ending != "answered" and caller_expected(r, shape):
        return Cause("expected", detail, setup_where)
    if any(e.ending == ending and e.code == code and e.early_media is not None for e in shape.expected):
        detail += " with early media" if jv.boolean(r, "early_media") else " without early media"
    if ending == "rejected" and code == OVERLOAD_CODE:
        if jv.opt_int(r, "retry_after_s") is not None:
            return Cause("overload", detail, setup_where)
        if setup_where != OUTSIDE:
            return Cause("dependency_fault", detail, setup_where)
    if ending == "timeout":
        return Cause("setup_timeout", detail, setup_where)
    if ending in ("cut", "stranded") or (ending == "failed" and jv.opt_number(r, "answered_at_ms") is not None):
        at = max((jv.number(f, "at_ms") for f in jv.objs(r, "failures")), default=ended)
        return Cause("cut", detail, places.where(at, at))
    return Cause("other", detail, setup_where)


@dataclass
class Tally:
    """The calls of a stream or a group: answered, and the others by cause, detail and place."""

    calls: int = 0
    answered: int = 0
    causes: Counter[Cause] = field(default_factory=lambda: Counter[Cause]())

    def add(self, c: Cause | None) -> None:
        self.calls += 1
        if c is None:
            self.answered += 1
        else:
            self.causes[c] += 1

    def merge(self, other: Tally) -> None:
        self.calls += other.calls
        self.answered += other.answered
        self.causes.update(other.causes)

    def count(self, cause: str) -> int:
        return sum(n for c, n in self.causes.items() if c.cause == cause)

    def share(self) -> float | None:
        """Answered or its expected ending, among the calls left once dependency-fault refusals are out."""
        rest = self.calls - self.count("dependency_fault")
        return None if rest == 0 else round((self.answered + self.count("expected")) / rest, 7)

    def doc(self) -> Json:
        rows: list[Json] = []
        for cause in CAUSES:
            details: dict[str, Counter[str]] = {}
            for c, n in self.causes.items():
                if c.cause == cause:
                    details.setdefault(c.detail, Counter[str]())[c.where] += n
            for detail, where in sorted(details.items()):
                rows.append({"cause": cause, "detail": detail, "count": where.total(), "where": dict(sorted(where.items()))})
        return {"calls": self.calls, "answered": self.answered, "causes": rows, "answered_share": self.share()}


def unanswered(
    records: Iterable[tuple[Json, Shape]], windows: list[Window], profile: Profile, shapes: dict[str, Shape]
) -> Json:
    """The `unanswered` section of a verdict: per stream with records, in the
    profile's order, then the `priority` and `others` groups of `run` streams."""
    places = Places(windows)
    per: dict[str, Tally] = {}
    for r, shape in records:
        per.setdefault(jv.string(r, "stream"), Tally()).add(cause_of(r, shape, places))
    order = [s.name for s in profile.streams] + sorted(set(per) - {s.name for s in profile.streams})
    streams: list[Json] = []
    groups = {PRIORITY: (Tally(), list[str]()), OTHERS: (Tally(), list[str]())}
    for name in order:
        t = per.get(name)
        if t is None:
            continue
        s = profile.stream(name)
        shape = shapes.get(s.shape) if s is not None else None
        prio = shape is not None and is_priority(shape)
        start = s.start if s is not None else "run"
        streams.append({"stream": name, "start": start, "priority": prio, **t.doc()})
        if start == "run":
            group, members = groups[PRIORITY if prio else OTHERS]
            group.merge(t)
            members.append(name)
    return {
        "streams": streams,
        "groups": [{"group": g, "streams": members, **t.doc()} for g, (t, members) in groups.items()],
    }
