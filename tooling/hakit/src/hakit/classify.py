"""The classifier: one call-outcome record and its catalog shape to impacts.

A pure function (ADR-0049 §4.6, the class table of §3.1). A record is a
success when its ending is in the shape's `expected` and every catalog leg the
generator saw ended as its leg expects. A setup that ends unanswered and
unexpected is `setup_failed` where the shape expects an answer, or where it
drew no final at all whatever the shape expects; a final the shape does not
expect is otherwise an `unexpected_ending`. A setup the generator gave up on
after hearing a provisional, 100 Trying included, is `silent_after_provisional`:
past it the caller sends nothing more (RFC 3261 §17.1.1.2), so nothing but its
own timer ends the wait, and no budget allows it. `producer_verdict` is never
read.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field, replace

from . import jsonval as jv
from .jsonval import Json
from .model import ExpectedEnding, Leg, Shape

SETUP_FAILED = "setup_failed"
SILENT_AFTER_PROVISIONAL = "silent_after_provisional"
UNEXPECTED_ENDING = "unexpected_ending"
SETUP_SLOW = "setup_slow"
DIALOG_REQUEST_FAILED = "dialog_request_failed"
CALL_CUT = "call_cut"
CALL_STRANDED = "call_stranded"
PROTOCOL_ANOMALY = "protocol_anomaly"
UNATTRIBUTED = "unattributed"

PRE_ANSWER_ENDINGS = ("rejected", "abandoned", "timeout", "failed")


@dataclass(frozen=True)
class Impact:
    cls: str
    stream: str
    call_id: str
    at_ms: float
    started_at_ms: float
    code: int | None = None
    reason: str | None = None
    answered: bool = False
    detail: str = ""
    # The final carried a `Retry-After` (the record's `retry_after_s` is set).
    retry_after: bool = False
    # How far a counted impact went past its own tolerance (a CDR duration).
    excess_ms: float = math.inf
    # A leg that opens after an in-dialog consult stayed unopened, and the
    # consult's asking leg received the contract's refusal (at `at_ms`).
    consult_refused: bool = False

    @property
    def budgetable(self) -> bool:
        """Takes a budget: a failed or unexpected setup before answer, or a
        consult's contract refusal on an otherwise clean call."""
        return self.cls == SETUP_FAILED or (self.cls == UNEXPECTED_ENDING and not self.answered) or self.consult_refused

    @property
    def setup_lost(self) -> bool:
        """The setup itself failed: whatever takes a budget, and the class none allows."""
        return self.budgetable or self.cls == SILENT_AFTER_PROVISIONAL


@dataclass(frozen=True)
class SetupLatency:
    """The setup time as `setup_slow` measures it, and the instant it was measured at."""

    stream: str
    call_id: str
    at_ms: float
    ms: float


@dataclass
class Classified:
    success: bool
    impacts: list[Impact] = field(default_factory=lambda: [])
    latency: SetupLatency | None = None
    generator_failure: str | None = None


def _matches(e: ExpectedEnding, ending: str, code: int | None, early_media: bool) -> bool:
    if e.ending != ending:
        return False
    if e.code is not None and e.code != code:
        return False
    return e.early_media is None or e.early_media == early_media


def _expected(options: list[ExpectedEnding], ending: str, code: int | None, early_media: bool) -> bool:
    return any(_matches(e, ending, code, early_media) for e in options)


def caller_expected(r: Json, shape: Shape) -> bool:
    """The caller's ending is one its shape expects."""
    return _expected(shape.expected, jv.string(r, "ending"), jv.opt_int(r, "final_code"), jv.boolean(r, "early_media"))


def _leg_impacts(r: Json, shape: Shape, base: Impact) -> list[Impact]:
    """A leg judged under the caller's ending. On an answered call a leg is
    known missing or wrong only once the call is over: its impact is at the
    call's end, so a fault in the middle of the call is among its candidates."""
    if not shape.legs or "legs" not in r:
        return []
    seen = {jv.string(leg, "role"): leg for leg in jv.objs(r, "legs")}
    caller = (jv.string(r, "ending"), jv.opt_int(r, "final_code"), jv.boolean(r, "early_media"))
    if base.answered:
        base = replace(base, at_ms=jv.number(r, "ended_at_ms"))
    out: list[Impact] = []
    for leg in shape.legs:
        if leg.when is not None and not _expected(leg.when, *caller):
            continue
        got = seen.get(leg.role)
        if got is None:
            unopened = replace(base, code=0, reason=f"leg:{leg.role}", detail="leg never opened", retry_after=False)
            out.append(_consult_refused(r, leg, seen, unopened))
            continue
        code = jv.opt_int(got, "final_code")
        ending = jv.string(got, "ending")
        if not _expected(leg.expected, ending, code, jv.boolean(got, "early_media")):
            out.append(
                replace(base, code=code or 0, reason=f"leg:{leg.role}", detail=f"leg ended {ending}", retry_after=False)
            )
    return out


def _consult_refused(r: Json, leg: Leg, seen: dict[str, Json], unopened: Impact) -> Impact:
    """The unopened leg of an answered call, charged at its consult's refusal
    when the leg opens after a consult (`after_consult`), its asking leg
    received one of the contract's refusals and the call holds no other
    failure; otherwise unchanged."""
    consult = leg.after_consult
    asking = seen.get(consult.by) if consult is not None else None
    if consult is None or asking is None or not unopened.answered or jv.objs(r, "failures"):
        return unopened
    for req in jv.objs(asking, "received"):
        if any(refusal.matches(req) for refusal in consult.refusals):
            return replace(unopened, at_ms=jv.number(req, "at_ms"), detail="consult refused", consult_refused=True)
    return replace(unopened, detail="leg never opened; its consult drew no contract refusal")


def setup_latency(r: Json) -> SetupLatency | None:
    """From `started_at_ms` to the first provisional, or to the final when none
    came; None when the setup never ended, or ended with no answer at all (the
    generator giving up measures nothing of the system under test)."""
    first_prov = jv.opt_number(r, "first_provisional_at_ms")
    unanswered = jv.opt_int(r, "final_code") is None and jv.string(r, "ending") != "answered"
    if first_prov is None and unanswered:
        return None
    measured_at = first_prov if first_prov is not None else jv.opt_number(r, "setup_ended_at_ms")
    if measured_at is None:
        return None
    started = jv.number(r, "started_at_ms")
    return SetupLatency(jv.string(r, "stream"), jv.string(r, "call_id"), measured_at, measured_at - started)


def silent_after_provisional(r: Json) -> bool:
    """The generator gave up with no final on a caller who had heard a
    provisional by then. One heard after the give-up leaves it a setup that
    heard nothing, the generator's CANCEL being all that came of it."""
    if jv.string(r, "ending") != "timeout" or jv.opt_int(r, "final_code") is not None:
        return False
    heard = [t for t in (jv.opt_number(r, "trying_at_ms"), jv.opt_number(r, "first_provisional_at_ms")) if t is not None]
    gave_up = jv.opt_number(r, "setup_ended_at_ms")
    return bool(heard) and (gave_up is None or min(heard) <= gave_up)


def classify(r: Json, shape: Shape) -> Classified:
    stream = jv.string(r, "stream")
    call_id = jv.string(r, "call_id")
    started = jv.number(r, "started_at_ms")
    ended = jv.number(r, "ended_at_ms")
    setup_end = jv.opt_number(r, "setup_ended_at_ms")
    answered_at = jv.opt_number(r, "answered_at_ms")
    ending = jv.string(r, "ending")
    code = jv.opt_int(r, "final_code")
    reason = jv.opt_string(r, "reason")
    failures = jv.objs(r, "failures")

    out = Classified(success=False)
    gen = [f for f in failures if jv.string(f, "kind") == "generator"]
    if gen:
        out.generator_failure = jv.string(gen[0], "detail", "generator failure")
        return out

    out.latency = setup_latency(r)

    setup_instant = setup_end if setup_end is not None else ended
    answered = answered_at is not None
    retry_after = jv.opt_int(r, "retry_after_s") is not None
    base = Impact(UNEXPECTED_ENDING, stream, call_id, setup_instant, started, answered=answered,
                  retry_after=retry_after)

    caller_ok = caller_expected(r, shape)
    if not caller_ok:
        if not answered and silent_after_provisional(r):
            out.impacts.append(replace(base, cls=SILENT_AFTER_PROVISIONAL, code=0, reason=reason or ending))
        elif ending in PRE_ANSWER_ENDINGS and not answered:
            cls = SETUP_FAILED if shape.expects_answer or code is None else UNEXPECTED_ENDING
            out.impacts.append(replace(base, cls=cls, code=code or 0, reason=reason or ending))
        elif ending == "cut":
            cut_at = max((jv.number(f, "at_ms") for f in failures), default=ended)
            out.impacts.append(replace(base, cls=CALL_CUT, at_ms=cut_at, detail="cut"))
        elif ending == "stranded":
            out.impacts.append(replace(base, cls=CALL_STRANDED, at_ms=ended, detail="teardown never completed"))
        else:
            out.impacts.append(replace(base, code=code or 0, reason=reason or ending, detail=f"ending {ending}"))
    leg_impacts = _leg_impacts(r, shape, base) if caller_ok else []
    out.impacts.extend(leg_impacts)

    for f in failures:
        kind = jv.string(f, "kind")
        cls = DIALOG_REQUEST_FAILED if kind == "dialog_request" else PROTOCOL_ANOMALY
        method = jv.opt_string(f, "method") or ""
        parts = (jv.opt_string(f, "leg") or "", method, str(jv.opt_int(f, "code") or ""), jv.opt_string(f, "detail") or "")
        detail = " ".join(x for x in parts if x)
        out.impacts.append(
            Impact(cls, stream, call_id, jv.number(f, "at_ms"), started, code=jv.opt_int(f, "code"), detail=detail)
        )
    out.success = caller_ok and not leg_impacts
    return out
