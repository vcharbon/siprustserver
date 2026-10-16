"""The Grafana dashboards of the search path and the block each one carries (ADR-0049 §7.5).

Every `*.json` below a `dashboards/` directory of the search path is one
dashboard, known by its `uid`; uid and title are each unique across the whole
path (Grafana provisions them all into one folder). Each carries the generic
incident block: the `Incident window` region annotation from
`sip_chaos_window == 1`, the `Incident phase` point annotation from
`sip_chaos_event`, both read from the VictoriaMetrics datasource and titled by
incident (and phase), the tag `hakit`, and the link listing every dashboard so
tagged with the time range carried.
"""

from __future__ import annotations

import json
from pathlib import Path

from .jsonval import Json, ShapeError, as_list, as_obj

TAG = "hakit"
# The host stack's VictoriaMetrics datasource (deploy/observability/stack).
DATASOURCE: Json = {"type": "prometheus", "uid": "vm"}

WINDOW_ANNOTATION: Json = {
    "name": "Incident window",
    "datasource": dict(DATASOURCE),
    "enable": True,
    "iconColor": "orange",
    "expr": 'max by (incident, run) (sip_chaos_window{incident!=""}) == 1',
    "titleFormat": "{{incident}}",
    "tagKeys": "incident,run",
    "step": "10s",
}

PHASE_ANNOTATION: Json = {
    "name": "Incident phase",
    "datasource": dict(DATASOURCE),
    "enable": True,
    "iconColor": "red",
    "expr": 'timestamp(sip_chaos_event{incident!=""}) * 1000',
    "useValueForTime": True,
    "titleFormat": "{{incident}} {{phase}}",
    "tagKeys": "incident,phase,source,run",
    "step": "10s",
}

LINK: Json = {
    "type": "dashboards",
    "title": "HA tooling dashboards",
    "tags": [TAG],
    "asDropdown": True,
    "keepTime": True,
    "includeVars": False,
    "targetBlank": False,
}

# What an entry must match; the rest (colour, step, tag keys) is the dashboard's.
_ANNOTATION_KEYS = ("name", "expr", "titleFormat", "enable", "useValueForTime")
_LINK_KEYS = ("type", "tags", "keepTime")


def _carrying(entries: list[Json], want: Json, keys: tuple[str, ...]) -> list[Json]:
    return [e for e in entries if all(e.get(k) == want.get(k) for k in keys)]


def _victoriametrics(entry: Json) -> bool:
    try:
        return as_obj(entry.get("datasource")) == DATASOURCE
    except ShapeError:
        return False


def block_errors(doc: Json) -> list[str]:
    """What the dashboard `doc` lacks of the generic block, one line each."""
    errs: list[str] = []
    listed = as_list(as_obj(doc.get("annotations") or {}).get("list") or [])
    annotations = [as_obj(a) for a in listed]
    for want in (WINDOW_ANNOTATION, PHASE_ANNOTATION):
        found = _carrying(annotations, want, _ANNOTATION_KEYS)
        if not found:
            errs.append(f"no {want['name']!r} annotation on {want['expr']}")
        elif not any(_victoriametrics(a) for a in found):
            errs.append(f"the {want['name']!r} annotation reads another datasource than VictoriaMetrics (uid vm)")
    if TAG not in as_list(doc.get("tags") or []):
        errs.append(f"no tag {TAG!r}")
    links = [as_obj(entry) for entry in as_list(doc.get("links") or [])]
    if not _carrying(links, LINK, _LINK_KEYS):
        errs.append(f"no link to the dashboards tagged {TAG!r} with the time range kept")
    return errs


def read_dashboard(p: Path) -> tuple[str | None, str, list[str]]:
    """The uid and title of the dashboard file `p`, and its refusals, each naming the file."""
    try:
        raw: object = json.loads(p.read_text())
    except (OSError, ValueError) as e:
        return None, "", [f"{p}: cannot read: {e}"]
    try:
        doc = as_obj(raw, "the dashboard")
    except ShapeError as e:
        return None, "", [f"{p}: {e}"]
    uid, title = doc.get("uid"), doc.get("title")
    if not isinstance(uid, str) or not uid:
        return None, "", [f"{p}: no uid"]
    if not isinstance(title, str) or not title:
        return None, "", [f"{p}: no title"]
    try:
        errs = block_errors(doc)
    except ShapeError as e:
        errs = [f"malformed: {e}"]
    return uid, title, [f"{p}: {e}" for e in errs]
