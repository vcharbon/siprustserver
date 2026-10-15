"""The judgement of a capacity ramp's steps: which steps are clean, the ramp's two
limits over them, and the worker CPU fitted per opened and per rejected call.
Pure functions over step rows (one JSON object per step).

A step is clean when every call the worker accepted completed within the setup
bound, whatever it shed:
  - the ramp's SIPp saw no other failure and no dead-call or out-of-call message;
  - the background SIPp streams saw no failure, their 503s aside, and no
    out-of-call message (their dead-call messages are a steady background on
    healthy steps and are left out);
  - the loadgen saw no failure: every class but ok is one, a 503 included
    (only a run declaring its overload refusal accepts it, as class ok);
  - the SIPp setup p99 is within the bound;
  - the worker's counts match what the callers saw, within TOL_REL of the count
    plus TOL_ABS cps: accepted = calls completed (ramp SIPp, background SIPp,
    loadgen ok but refused for overload) and rejected = refusals seen (ramp
    SIPp shed, background SIPp, loadgen status_503 and refused for overload).
    A 503 the call path authors after accepting a call counts as a failure at
    the caller and as accepted at the worker, so it breaks the match.
A step missing any of these is not judged.

Row fields: `label`; `sipp_split` {cps, n} of completed, shed_503, other_fail,
dead_call, out_of_call; `soak_sipp` {completed_cps, failed, shed_503,
shed_503_cps, out_of_call}; `loadgen_ok_cps`, `loadgen_503_cps`,
`loadgen_refused_overload_cps`, `loadgen_not_ok` {class: n};
`sipp_setup_ms_q` {"0.99": "<200" | ">=1000"}; `worker_accepted_cps`,
`worker_cancelled_cps`, `worker_rejected_cps`, `worker_cpu_cores`,
`worker_throttled_s`, `worker_cpu_limit_cores`.
"""

from __future__ import annotations

import math
from collections.abc import Mapping, Sequence

from ..jsonval import Json

# The worker counts a call at admission, the caller at its end; over a steady
# step the two rates agree within this.
TOL_REL = 0.02
TOL_ABS = 2.0
# |cos(accepted, rejected)| above which the fit cannot tell the two costs apart.
COLLINEAR = 0.9
# A step whose cores reach this share of the worker's CPU limit is saturated.
SATURATED = 0.9


def _obj(r: Mapping[str, object] | None, key: str) -> Json | None:
    v = None if r is None else r.get(key)
    return {str(k): x for k, x in v.items()} if isinstance(v, dict) else None  # pyright: ignore[reportUnknownVariableType, reportUnknownArgumentType]


def _num(r: Mapping[str, object] | None, key: str) -> float | None:
    v = None if r is None else r.get(key)
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def p99_ms(r: Json) -> float | None:
    """The SIPp setup p99's repartition bound in ms ("<200" -> 200, ">=1000" -> inf)."""
    q = _obj(r, "sipp_setup_ms_q") or {}
    b = q.get("0.99")
    if not isinstance(b, str) or not b:
        return None
    return math.inf if b.startswith(">=") else float(b.lstrip("<"))


def completed_cps(r: Json | None) -> float | None:
    """Calls completed through the worker; None without the ramp's split."""
    cps = _obj(_obj(r, "sipp_split"), "cps")
    done = _num(cps, "completed")
    if done is None:
        return None
    return done + (_num(_obj(r, "soak_sipp"), "completed_cps") or 0.0) + (_num(r, "loadgen_ok_cps") or 0.0)


def shed_seen_cps(r: Json) -> float | None:
    """Refusals the callers saw; None when a source's 503s cannot be told from its failures."""
    shed = _num(_obj(_obj(r, "sipp_split"), "cps"), "shed_503")
    if shed is None:
        return None
    soak = _obj(r, "soak_sipp")
    if soak and _num(soak, "shed_503_cps") is None:
        return None
    return (
        shed
        + (_num(soak, "shed_503_cps") or 0.0)
        + (_num(r, "loadgen_503_cps") or 0.0)
        + (_num(r, "loadgen_refused_overload_cps") or 0.0)
    )


def judge(r: Json, p99_bound: float) -> tuple[bool | None, str]:
    """(clean, why): clean is None when the step cannot be judged."""
    n = _obj(_obj(r, "sipp_split"), "n")
    other = _num(n, "other_fail")
    if n is None or other is None:
        return None, "no SIPp split"
    acc, rej = _num(r, "worker_accepted_cps"), _num(r, "worker_rejected_cps")
    if acc is None or rej is None:
        return None, "no new-call counts"
    p99 = p99_ms(r)
    if p99 is None:
        return None, "no setup p99"
    why: list[str] = []
    if other >= 0.5:
        why.append(f"SIPp other failures {other:.0f}")
    stray = (_num(n, "dead_call") or 0) + (_num(n, "out_of_call") or 0)
    if stray >= 0.5:
        why.append(f"SIPp dead-call/out-of-call messages {stray:.0f}")
    soak = _obj(r, "soak_sipp")
    if soak:
        sfail = (_num(soak, "failed") or 0) - (_num(soak, "shed_503") or 0)
        if sfail >= 0.5:
            why.append(f"background SIPp failures {sfail:.0f}")
        if (_num(soak, "out_of_call") or 0) >= 0.5:
            why.append(f"background SIPp out-of-call messages {_num(soak, 'out_of_call'):.0f}")
    lgf = {k: v for k, v in (_obj(r, "loadgen_not_ok") or {}).items() if isinstance(v, (int, float))}
    if lgf:
        why.append("loadgen " + ", ".join(f"{k} {v:.0f}" for k, v in lgf.items()))
    if p99 > p99_bound:
        why.append(f"setup p99 {(_obj(r, 'sipp_setup_ms_q') or {}).get('0.99')} ms")
    for what, worker, seen in (("accepted", acc, completed_cps(r)), ("rejected", rej, shed_seen_cps(r))):
        if seen is None:
            return None, f"no caller-side count of the {what} calls"
        tol = TOL_REL * max(worker, seen) + TOL_ABS
        if abs(worker - seen) > tol:
            why.append(f"worker {what} {worker:.1f} cps vs callers {seen:.1f} (tolerance {tol:.1f})")
    return not why, "; ".join(why)


def limits(rows: Sequence[Json], p99_bound: float) -> Json:
    """The accepted-call capacity (highest completed cps over the clean steps) and
    the reject throughput (highest worker rejected cps over them)."""
    verdicts = {str(r["label"]): judge(r, p99_bound) for r in rows}
    clean = [r for r in rows if verdicts[str(r["label"])][0]]
    acc = max(clean, key=lambda r: completed_cps(r) or 0, default=None)
    rej = max(clean, key=lambda r: _num(r, "worker_rejected_cps") or 0, default=None)
    rej_cps = _num(rej, "worker_rejected_cps")
    return {
        "p99_ms": p99_bound,
        "accepted_call_capacity": {"cps": completed_cps(acc), "step": acc["label"]} if acc else None,
        "reject_throughput": {"cps": rej_cps, "step": rej["label"]} if rej and rej_cps else None,
        "steps": {label: {"clean": c, "why": why} for label, (c, why) in verdicts.items()},
    }


def unsaturated(r: Json) -> bool:
    """Not throttled, and below SATURATED of the CPU limit when there is one."""
    if (_num(r, "worker_throttled_s") or 0) >= 0.5:
        return False
    limit = _num(r, "worker_cpu_limit_cores")
    return not limit or (_num(r, "worker_cpu_cores") or 0) < SATURATED * limit


def opened_cps(r: Json) -> float:
    """Accepted calls and those cancelled by their caller while their turn waited."""
    return (_num(r, "worker_accepted_cps") or 0.0) + (_num(r, "worker_cancelled_cps") or 0.0)


def cpu_fit(rows: Sequence[Json], clean: Mapping[str, bool | None]) -> Json | None:
    """cores = a * opened cps + b * rejected cps, least squares with no intercept, over
    the clean, unsaturated steps. a and b are None when the steps cannot separate
    the two costs: no step rejected (b only), or accepted and rejected collinear
    (|cos| > COLLINEAR). `flags` names a negative cost. Per step, the residual."""
    pts: list[tuple[str, float, float, float]] = []
    for r in rows:
        acc, rej, cores = _num(r, "worker_accepted_cps"), _num(r, "worker_rejected_cps"), _num(r, "worker_cpu_cores")
        if not clean.get(str(r["label"])) or acc is None or rej is None or cores is None:
            continue
        if opened_cps(r) + rej > 0 and unsaturated(r):
            pts.append((str(r["label"]), opened_cps(r), rej, cores))
    if len(pts) < 2:
        return None
    saa = sum(x * x for _, x, _, _ in pts)
    srr = sum(z * z for _, _, z, _ in pts)
    sar = sum(x * z for _, x, z, _ in pts)
    say = sum(x * y for _, x, _, y in pts)
    sry = sum(z * y for _, _, z, y in pts)
    cos = sar / math.sqrt(saa * srr) if saa > 0 and srr > 0 else None
    note: str | None = None
    flags: list[str] = []
    ca: float | None
    cb: float | None
    if srr == 0:
        ca, cb, note = say / saa, None, "no fitted step rejected"
    elif cos is not None and abs(cos) > COLLINEAR:
        ca = cb = None
        note = f"accepted and rejected are collinear (cos {cos:.3f}): the steps cannot tell their costs apart"
    else:
        det = saa * srr - sar * sar
        ca, cb = (say * srr - sry * sar) / det, (saa * sry - sar * say) / det
    for name, v in (("a", ca), ("b", cb)):
        if v is not None and v < 0:
            flags.append(f"{name} < 0: not a cost, the fit does not hold")
    res = {label: y - ((ca or 0) * x + (cb or 0) * z) for label, x, z, y in pts} if ca is not None else {}
    ys = [y for *_, y in pts]
    ybar = sum(ys) / len(ys)
    ss_res = sum(v * v for v in res.values())
    ss_tot = sum((y - ybar) ** 2 for y in ys)
    return {
        "steps": len(pts),
        "cos_accepted_rejected": cos,
        "ms_per_accepted_call": 1000 * ca if ca is not None else None,
        "ms_per_rejected_call": 1000 * cb if cb is not None else None,
        "r2": 1 - ss_res / ss_tot if res and ss_tot > 0 else None,
        "rms_residual_cores": math.sqrt(ss_res / len(pts)) if res else None,
        "residual_cores": res,
        "note": note,
        "flags": flags,
    }
