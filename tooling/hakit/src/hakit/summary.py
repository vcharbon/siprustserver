"""`summary.md`: the human rendering of a `verdict.json`."""

from __future__ import annotations

from datetime import UTC, datetime

from . import jsonval as jv
from .jsonval import Json


def _t(ms: float) -> str:
    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%H:%M:%S.%f")[:-3]


def _impact(r: Json) -> str:
    key = " ".join(
        str(x) for x in (r.get("stream"), r.get("code"), r.get("reason")) if x is not None and x != ""
    )
    secs = f" ({r['seconds']} s of offered rate)" if "seconds" in r else ""
    mark = "FINDING" if r.get("finding") is True else "ok"
    return f"- `{r['class']}` {key}: {r['count']}{secs}, allowed {r['allowed']} — {mark}"


def render(v: Json) -> str:
    out = [
        f"# {jv.string(v, 'run')}",
        "",
        f"Profile `{jv.string(v, 'profile')}`, status **{jv.string(v, 'status')}**, "
        f"{_t(jv.number(v, 'started_at_ms'))} to {_t(jv.number(v, 'ended_at_ms'))} UTC.",
        "",
        "## Streams",
        "",
        "| stream | shape | cps | calls | records | success |",
        "|---|---|---|---|---|---|",
    ]
    for s in jv.objs(v, "streams"):
        out.append(
            f"| {s['stream']} | {s['shape']} | {s['cps']} | {s['calls']} | {s['records']} | {s['success']} |"
        )
    out += ["", "## Windows", ""]
    for w in jv.objs(v, "windows"):
        out.append(
            f"### {w['event']} ({w['status']}, budget `{w['budget']}`): "
            f"{_t(jv.number(w, 'opened_at_ms'))} to {_t(jv.number(w, 'closed_at_ms'))}, {w['findings']} findings"
        )
        out.append("")
        for p in jv.objs(w, "phases"):
            out.append(f"- {_t(jv.number(p, 'at_ms'))} {p['step']} {p['phase']}")
        rows = jv.objs(w, "impacts")
        out += [_impact(r) for r in rows] or ["- no impact"]
        out.append("")
    out += ["## In-dialog impacts", ""]
    in_dialog = jv.objs(v, "in_dialog")
    out += [
        f"- `{i['class']}` {i['stream']} {i['call_id']} at {_t(jv.number(i, 'at_ms'))}, windows {', '.join(jv.strings(i, 'candidate_windows'))}"
        for i in in_dialog
    ] or ["- none"]
    out += ["", "## Unattributed", ""]
    out += [_impact(r) for r in jv.objs(v, "unattributed")] or ["- none"]
    out += ["", "## Integrity", ""]
    out += [
        f"- `{i['kind']}` {i.get('stream') or i.get('event') or ''}: {i['detail']}" for i in jv.objs(v, "integrity")
    ] or ["- none"]
    return "\n".join(out) + "\n"
