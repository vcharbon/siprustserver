"""The classifier: one call-outcome record and its catalog shape to impacts.

A pure function (ADR-0049 §4.6, the class table of §3.1). A record is a
success when its ending is in the shape's `expected` and every catalog leg the
generator saw ended as its leg expects. `producer_verdict` is never read.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace

from . import jsonval as jv
from .jsonval import Json
from .model import ExpectedEnding, Shape

SETUP_FAILED = "setup_failed"
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

    @property
    def budgetable(self) -> bool:
        """Takes a budget: a failed or unexpected setup before answer."""
        return self.cls == SETUP_FAILED or (self.cls == UNEXPECTED_ENDING and not self.answered)


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


def _leg_impacts(r: Json, shape: Shape, base: Impact) -> list[Impact]:
    if not shape.legs or "legs" not in r:
        return []
    seen = {jv.string(leg, "role"): leg for leg in jv.objs(r, "legs")}
    out: list[Impact] = []
    for leg in shape.legs:
        got = seen.get(leg.role)
        if got is None:
            out.append(replace(base, code=0, reason=f"leg:{leg.role}", detail="leg never opened"))
            continue
        code = jv.opt_int(got, "final_code")
        ending = jv.string(got, "ending")
        if not _expected(leg.expected, ending, code, jv.boolean(got, "early_media")):
            out.append(replace(base, code=code or 0, reason=f"leg:{leg.role}", detail=f"leg ended {ending}"))
    return out


def classify(r: Json, shape: Shape) -> Classified:
    stream = jv.string(r, "stream")
    call_id = jv.string(r, "call_id")
    started = jv.number(r, "started_at_ms")
    ended = jv.number(r, "ended_at_ms")
    setup_end = jv.opt_number(r, "setup_ended_at_ms")
    first_prov = jv.opt_number(r, "first_provisional_at_ms")
    answered_at = jv.opt_number(r, "answered_at_ms")
    ending = jv.string(r, "ending")
    code = jv.opt_int(r, "final_code")
    reason = jv.opt_string(r, "reason")
    early_media = jv.boolean(r, "early_media")
    failures = jv.objs(r, "failures")

    out = Classified(success=False)
    gen = [f for f in failures if jv.string(f, "kind") == "generator"]
    if gen:
        out.generator_failure = jv.string(gen[0], "detail", "generator failure")
        return out

    measured_at = first_prov if first_prov is not None else setup_end
    if measured_at is not None:
        out.latency = SetupLatency(stream, call_id, measured_at, measured_at - started)

    setup_instant = setup_end if setup_end is not None else ended
    answered = answered_at is not None
    base = Impact(UNEXPECTED_ENDING, stream, call_id, setup_instant, started, answered=answered)

    caller_ok = _expected(shape.expected, ending, code, early_media)
    if not caller_ok:
        if ending in PRE_ANSWER_ENDINGS and not answered:
            cls = SETUP_FAILED if shape.expects_answer else UNEXPECTED_ENDING
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
        detail = " ".join(x for x in (method, str(jv.opt_int(f, "code") or ""), jv.opt_string(f, "detail") or "") if x)
        out.impacts.append(
            Impact(cls, stream, call_id, jv.number(f, "at_ms"), started, code=jv.opt_int(f, "code"), detail=detail)
        )
    out.success = caller_ok and not leg_impacts
    return out
