"""`hakit obs-check <scope>...`: is every series a lab needs in VictoriaMetrics, fresh?

A scope is a list of selectors and of the scopes it includes; `${namespace}`
stands for the lab namespace. The base scopes cover the in-cluster collectors
(`cluster`), the system under test the base chart deploys (`sut`) and its
front proxy, when the release runs one (`proxy`, which includes `sut`). A product
adds its own scopes with `--spec FILE` (YAML: `scopes: {name: {include: [...],
series: [...]}}`); a scope name the base defines is refused. The check itself
is `deploy/observability/check.sh`, which waits for each selector to have a
fresh sample and names the missing ones.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import yaml

from .. import jsonval as jv
from ..executor import Executor, ProcResult
from ..jsonval import as_obj
from ..paths import REPO_ROOT

CHECK = REPO_ROOT / "deploy" / "observability" / "check.sh"


@dataclass(frozen=True)
class Scope:
    include: list[str] = field(default_factory=lambda: [])
    series: list[str] = field(default_factory=lambda: [])


BASE_SCOPES: dict[str, Scope] = {
    "cluster": Scope(
        series=[
            "kube_pod_info",
            "container_cpu_usage_seconds_total",
            "container_memory_rss",
            "node_cpu_seconds_total",
            'up{job="kubernetes-pods"}',
        ]
    ),
    "sut": Scope(
        include=["cluster"],
        series=[
            'kube_pod_info{exported_namespace="${namespace}"}',
            'container_memory_rss{namespace="${namespace}"}',
            'b2bua_active_calls{namespace="${namespace}"}',
            'limiter_admission_max{namespace="${namespace}"}',
            'cdr_consumed_total{namespace="${namespace}"}',
        ],
    ),
    "proxy": Scope(include=["sut"], series=['sip_proxy_requests_total{namespace="${namespace}"}']),
}


def load_scopes(spec: Path | None) -> dict[str, Scope]:
    scopes = dict(BASE_SCOPES)
    if spec is None:
        return scopes
    d = as_obj(yaml.safe_load(spec.read_text()) or {}, str(spec))
    for name, raw in jv.obj(d, "scopes").items():
        if name in BASE_SCOPES:
            raise ValueError(f"{spec}: scope {name!r} is the base's")
        s = as_obj(raw, name)
        scopes[name] = Scope(include=jv.strings(s, "include"), series=jv.strings(s, "series"))
    return scopes


def selectors(scopes: dict[str, Scope], names: list[str], namespace: str) -> list[str]:
    """The selectors of `names` and every scope they include, each once, in order."""
    out: list[str] = []
    seen: set[str] = set()

    def visit(name: str, path: tuple[str, ...]) -> None:
        if name in path:
            raise ValueError(f"scope {name!r} includes itself through {' -> '.join(path)}")
        if name in seen:
            return
        scope = scopes.get(name)
        if scope is None:
            raise ValueError(f"unknown scope {name!r} (known: {', '.join(sorted(scopes))})")
        seen.add(name)
        for inc in scope.include:
            visit(inc, (*path, name))
        for sel in scope.series:
            s = sel.replace("${namespace}", namespace)
            if s not in out:
                out.append(s)

    for n in names:
        visit(n, ())
    return out


def check(executor: Executor, vm: str, series: list[str], wait_s: float) -> ProcResult:
    return executor.run(
        [str(CHECK), "--wait", f"{wait_s:g}", *series], env={"VM": vm}, timeout_s=wait_s + 120
    )
