"""Where a profile needs its pods (`placement`), read from the API server before
the load: a run whose fault takes a node means to take what runs on it and no
more, so a pod elsewhere refuses the run. Each entry's selector must name at
least one pod, and every one of them must run on a node of its node selector.
"""

from __future__ import annotations

import json

from . import jsonval as jv
from .executor import Executor
from .lab import Lab
from .profile import Placement, Profile


def _names(listing: str, field: str) -> dict[str, str]:
    """Each item of a `kubectl get -o json` answer by its name: its `spec`'s
    `field`, or its name when `field` is empty."""
    out: dict[str, str] = {}
    for item in jv.objs(jv.as_obj(json.loads(listing)), "items"):
        name = jv.string(jv.obj(item, "metadata"), "name")
        out[name] = jv.string(jv.obj(item, "spec"), field) if field else name
    return out


def _errors(spec: Placement, lab: Lab, executor: Executor) -> list[str]:
    where = f"placement {spec.selector!r}"
    kube = ["kubectl", "--context", lab.kube_context]
    pods = executor.run([*kube, "-n", lab.namespace, "get", "pods", "-l", spec.selector, "-o", "json"], timeout_s=30)
    nodes = executor.run([*kube, "get", "nodes", "-l", spec.node_selector, "-o", "json"], timeout_s=30)
    if pods.rc != 0 or nodes.rc != 0:
        return [f"{where}: cannot be read: {(pods.stderr + nodes.stderr).strip()[:200]}"]
    try:
        on_node, admitted = _names(pods.stdout, "nodeName"), _names(nodes.stdout, "")
    except (ValueError, jv.ShapeError) as e:
        return [f"{where}: the API server's answer is no list: {e}"]
    if not on_node:
        return [f"{where}: no pod matches"]
    return [
        f"{where}: {pod} runs on {node}, outside node selector {spec.node_selector!r}"
        for pod, node in sorted(on_node.items())
        if node not in admitted
    ]


def placement_errors(profile: Profile, lab: Lab, executor: Executor) -> list[str]:
    return [e for spec in profile.placement for e in _errors(spec, lab, executor)]
