"""`summary.md`: the human rendering of a `verdict.json` (ADR-0049 §3.11).

The status and why first (each integrity finding, each impact found), then the
streams with their generator health, then per window its phases, facts,
recovery per stream and every class against its allowance, then the in-dialog
and unattributed impacts.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime

from . import jsonval as jv
from .jsonval import Json


def _t(ms: float) -> str:
    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%H:%M:%S.%f")[:-3]


def _key(r: Json) -> str:
    ra = r.get("retry_after")
    tag = ("Retry-After" if ra else "no Retry-After") if isinstance(ra, bool) else None
    return " ".join(str(x) for x in (r.get("stream"), r.get("code"), r.get("reason"), tag) if x is not None and x != "")


def _impact(r: Json) -> str:
    secs = f" ({r['seconds']} s of offered rate)" if "seconds" in r else ""
    unit = " s" if r["class"] == "recovery" else ""
    mark = "FINDING" if r.get("finding") is True else "ok"
    key = _key(r)
    return f"- `{r['class']}`{' ' + key if key else ''}: {r['count']}{unit}{secs}, allowed {r['allowed']}{unit} — {mark}"


def _budget(w: Json) -> str:
    return f"budget `{w['budget']}`" if w["budget"] else "no budget (allows nothing)"


def _in_dialog_of(v: Json, event: str) -> dict[str, int]:
    """The in-dialog impacts that name `event` among their candidate windows, by class and stream."""
    out: dict[str, int] = {}
    for i in jv.objs(v, "in_dialog"):
        if event in jv.strings(i, "candidate_windows"):
            key = f"`{i['class']}` {i['stream']}"
            out[key] = out.get(key, 0) + 1
    return out


def _why(v: Json) -> list[str]:
    status = jv.string(v, "status")
    if status == "invalid":
        return [_integrity(i) for i in jv.objs(v, "integrity")]
    out: list[str] = []
    for w in jv.objs(v, "windows"):
        out += [f"- window {w['event']}: {_impact(r)[2:]}" for r in jv.objs(w, "impacts") if r.get("finding") is True]
        dialog = _in_dialog_of(v, str(w["event"]))
        if dialog:
            listed = ", ".join(f"{k} x{n}" for k, n in sorted(dialog.items()))
            out.append(f"- window {w['event']} is a candidate of {sum(dialog.values())} in-dialog impacts: {listed} — FINDING")
    n_dialog = len(jv.objs(v, "in_dialog"))
    if n_dialog:
        out.append(f"- {n_dialog} in-dialog impacts in all")
    out += [f"- unattributed: {_impact(r)[2:]}" for r in jv.objs(v, "unattributed")]
    return out or ["- every impact within its allowance, measurement intact"]


def _streams(v: Json) -> list[str]:
    out = [
        "| stream | shape | cps | calls | records | success | stalls (spoiled) |",
        "|---|---|---|---|---|---|---|",
    ]
    for s in jv.objs(v, "streams"):
        stalls = jv.objs(s, "stalls")
        spoiled = sum(int(jv.number(x, "spoiled")) for x in stalls)
        out.append(
            f"| {s['stream']} | {s['shape']} | {s['cps']} | {s['calls']} | {s['records']} | {s['success']} "
            f"| {len(stalls)} ({spoiled}) |"
        )
    return out


def _window(w: Json, dialog: dict[str, int]) -> list[str]:
    candidates = f", candidate of {sum(dialog.values())} in-dialog impacts" if dialog else ""
    out = [
        f"### {w['event']} ({w['source']}, {w['status']}, {_budget(w)}): "
        f"{_t(jv.number(w, 'opened_at_ms'))} to {_t(jv.number(w, 'closed_at_ms'))}, {w['findings']} findings{candidates}",
        "",
    ]
    out += [f"- {_t(jv.number(p, 'at_ms'))} {p['step']} {p['phase']}" for p in jv.objs(w, "phases")]
    for step, by_exe in jv.obj(w, "facts").items():
        for exe, facts in jv.as_obj(by_exe).items():
            out.append(f"- facts of {step} {exe}: `{json.dumps(facts, sort_keys=True)}`")
    for r in jv.objs(w, "recovery"):
        got = r.get("recovery_s")
        out.append(f"- {r['stream']} back {'never' if got is None else f'{got} s after healed'}")
    rows = jv.objs(w, "impacts")
    out += [_impact(r) for r in rows] or ["- no impact"]
    out += [f"- in-dialog {k}: {n} (this window a candidate) — FINDING" for k, n in sorted(dialog.items())]
    out.append("")
    return out


def render(v: Json) -> str:
    out = [
        f"# {jv.string(v, 'run')}",
        "",
        f"Profile `{jv.string(v, 'profile')}`, status **{jv.string(v, 'status')}**, "
        f"{_t(jv.number(v, 'started_at_ms'))} to {_t(jv.number(v, 'ended_at_ms'))} UTC.",
        "",
        "## Why",
        "",
        *_why(v),
        "",
        "## Streams",
        "",
        *_streams(v),
        "",
        "## Windows",
        "",
    ]
    for w in jv.objs(v, "windows"):
        out += _window(w, _in_dialog_of(v, str(w["event"])))
    out += ["## In-dialog impacts", ""]
    out += [
        f"- `{i['class']}` {i['stream']} {i['call_id']} at {_t(jv.number(i, 'at_ms'))}, "
        f"windows {', '.join(jv.strings(i, 'candidate_windows'))}"
        for i in jv.objs(v, "in_dialog")
    ] or ["- none"]
    out += ["", "## Unattributed", ""]
    out += [_impact(r) for r in jv.objs(v, "unattributed")] or ["- none"]
    out += ["", "## Integrity", ""]
    out += [_integrity(i) for i in jv.objs(v, "integrity")] or ["- none"]
    return "\n".join(out) + "\n"


def _integrity(i: Json) -> str:
    ids = jv.strings(i, "call_ids")
    calls = f" (calls {', '.join(ids[:5])}{', …' if len(ids) > 5 else ''})" if ids else ""
    return f"- `{i['kind']}` {i.get('stream') or i.get('event') or ''}: {i['detail']}{calls}"
