"""`hakit mark`: the state of a workflow supervising a run, pushed for the
dashboards (the `sip_soak_*` series of the base's metric catalogue) and as a
Grafana annotation, so a board tells a lab still coming up from a system under
test that is down.

  hakit mark phase <run> <phase>          one of PHASES; every phase is written, the others at 0
  hakit mark progress <run> <seen> <expected>
  hakit mark finding <run> <signature> <severity>
  hakit mark annotate <text> [<tag,tag>]

Best effort: a push or an annotation that fails is said on stderr and the
command still exits 0, so a mark never stops what it reports on.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Sequence

from .executor import Executor
from .vm import VictoriaMetrics, sample

PHASES = ("bringup", "launching", "running", "investigating", "remediating", "reporting", "done")


def lines(kind: str, args: Sequence[str], at_ms: int) -> list[str]:
    """The series lines of a mark; a ValueError names a malformed one."""
    if kind == "phase" and len(args) == 2:
        run, want = args
        if want not in PHASES:
            raise ValueError(f"phase {want!r} is not one of {', '.join(PHASES)}")
        out = [sample("sip_soak_phase", {"run": run, "phase": p}, 1 if p == want else 0, at_ms) for p in PHASES]
        return [*out, sample("sip_soak_up", {"run": run}, 1, at_ms)]
    if kind == "progress" and len(args) == 3:
        run, seen, expected = args
        return [
            sample("sip_soak_events_seen", {"run": run}, float(seen), at_ms),
            sample("sip_soak_events_expected", {"run": run}, float(expected), at_ms),
        ]
    if kind == "finding" and len(args) == 3:
        run, sig, sev = args
        return [sample("sip_soak_finding", {"run": run, "signature": sig, "severity": sev}, 1, at_ms)]
    if kind == "annotate" and 1 <= len(args) <= 2:
        return []
    raise ValueError(f"usage: hakit mark {{phase <run> <phase>|progress <run> <seen> <expected>|finding <run> <sig> <sev>|annotate <text> [tags]}}, got {kind} {list(args)}")


def annotation(kind: str, args: Sequence[str]) -> tuple[str, list[str]] | None:
    """The Grafana annotation of a mark: its text and tags."""
    if kind == "phase":
        return f"soak {args[0]}: {args[1]}", ["soak", "phase"]
    if kind == "finding":
        return f"finding [{args[2]}] {args[1]}", ["soak", "finding"]
    if kind == "annotate":
        tags = args[1].split(",") if len(args) > 1 else ["soak"]
        return args[0], [t for t in tags if t]
    return None


def mark(executor: Executor, vm_url: str, grafana_url: str, kind: str, args: Sequence[str], at_ms: int) -> int:
    try:
        series = lines(kind, args, at_ms)
    except ValueError as e:
        print(f"hakit mark: {e}", file=sys.stderr)
        return 2
    if series and not VictoriaMetrics(executor, vm_url).push(series):
        print(f"hakit mark: the push to {vm_url} failed", file=sys.stderr)
    note = annotation(kind, args)
    if note is not None:
        body = json.dumps({"text": note[0], "tags": note[1], "time": at_ms}).encode()
        res = executor.http("POST", f"{grafana_url}/api/annotations", body, timeout_s=5, content_type="application/json")
        if not 200 <= res.status < 300:
            print(f"hakit mark: the annotation was refused: HTTP {res.status}", file=sys.stderr)
    return 0
