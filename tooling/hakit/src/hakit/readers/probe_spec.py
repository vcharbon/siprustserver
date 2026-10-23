"""What `hakit probe` checks beyond the run directory, with the base's defaults:
the system under test the base charts deploy. A product passes `--spec FILE`
(YAML, every key optional) for its own terms.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from pathlib import Path

import yaml

from .. import jsonval as jv
from ..jsonval import as_obj


@dataclass(frozen=True)
class Bound:
    """An instant query whose value must stay at or under `max` while the run runs."""

    name: str
    expr: str
    max: float


@dataclass(frozen=True)
class ProbeSpec:
    namespace: str = "sip-test"
    # Streams whose records may pause for minutes by design (long holds): left
    # out of the record liveness check.
    ballast: list[str] = field(default_factory=lambda: [])
    # Pods whose container restarts are reported; empty is every pod of the namespace.
    restart_pods: str = ""
    # The system under test's live calls, in total and by pod.
    active_calls: str = "b2bua_active_calls"
    bounds: list[Bound] = field(default_factory=lambda: [])
    # The records read back: those that ended in the last `recent_s`.
    recent_s: float = 900


def load_spec(path: Path | None) -> ProbeSpec:
    """The base spec, overridden by the keys of the YAML file at `path`."""
    spec = ProbeSpec()
    if path is None:
        return spec
    d = as_obj(yaml.safe_load(path.read_text()) or {}, str(path))
    bounds = spec.bounds
    if "bounds" in d:
        bounds = [Bound(jv.string(b, "name"), jv.string(b, "expr"), jv.number(b, "max")) for b in jv.objs(d, "bounds")]
    return replace(
        spec,
        namespace=jv.string(d, "namespace", spec.namespace),
        ballast=jv.strings(d, "ballast") if "ballast" in d else spec.ballast,
        restart_pods=jv.string(d, "restart_pods", spec.restart_pods),
        active_calls=jv.string(d, "active_calls", spec.active_calls),
        bounds=bounds,
        recent_s=jv.number(d, "recent_s", spec.recent_s),
    )
