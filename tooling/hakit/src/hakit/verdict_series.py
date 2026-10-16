"""The verdict as series, for the dashboards (ADR-0049 §7.5).

When a verdict is written, each window is pushed as
`sip_chaos_window_verdict{run,event,budget}` and `sip_chaos_window_findings`
(its impacts above their allowance), and the run as
`sip_chaos_run_verdict{run,profile}`, all at the instant the verdict is
computed. A verdict value is `0` pass, `1` fail, `2` invalid; a window whose
incident failed is invalid. A recomputed verdict pushes later samples of the
same series. The series are for display: `verdict.json` stays the verdict.
"""

from __future__ import annotations

from . import jsonval as jv
from .jsonval import Json
from .vm import sample

VERDICT_VALUE = {"pass": 0, "fail": 1, "invalid": 2}


def window_verdict(w: Json) -> str:
    if jv.string(w, "status") == "incident_failed":
        return "invalid"
    return "fail" if jv.number(w, "findings") > 0 else "pass"


def verdict_lines(v: Json, at_ms: int) -> list[str]:
    run = jv.string(v, "run")
    out: list[str] = []
    for w in jv.objs(v, "windows"):
        labels = {"run": run, "event": jv.string(w, "event"), "budget": jv.string(w, "budget")}
        out.append(sample("sip_chaos_window_verdict", labels, VERDICT_VALUE[window_verdict(w)], at_ms))
        out.append(sample("sip_chaos_window_findings", labels, jv.number(w, "findings"), at_ms))
    status = jv.string(v, "status")
    out.append(sample("sip_chaos_run_verdict", {"run": run, "profile": jv.string(v, "profile")}, VERDICT_VALUE[status], at_ms))
    return out
