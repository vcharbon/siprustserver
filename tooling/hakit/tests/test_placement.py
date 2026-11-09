"""A run that needs its pods on given nodes reads them before the load."""

from __future__ import annotations

import json

from hakit.executor import FakeExecutor, ProcResult
from hakit.lab import lab_from_env
from hakit.placement import placement_errors
from hakit.profile import parse_profile


def profile() -> object:
    return parse_profile(
        {
            "name": "p",
            "duration_s": 1,
            "setup_slow_ms": 1,
            "streams": [],
            "placement": [{"selector": "app=mock", "node_selector": "tier=infra"}],
        }
    )


def pods(*on: tuple[str, str]) -> str:
    return json.dumps({"items": [{"metadata": {"name": n}, "spec": {"nodeName": node}} for n, node in on]})


def errors(pod_list: str, node_list: list[str], rc: int = 0) -> list[str]:
    nodes = json.dumps({"items": [{"metadata": {"name": n}} for n in node_list]})

    def rule(call: object) -> ProcResult | None:
        argv = getattr(call, "argv", [])
        if "pods" in argv:
            return ProcResult(rc, pod_list, "no API" if rc else "")
        if "nodes" in argv:
            return ProcResult(0, nodes, "")
        return None

    p = profile()
    return placement_errors(p, lab_from_env({}), FakeExecutor([rule]))  # type: ignore[arg-type]


def test_every_pod_on_a_node_of_its_selector_passes() -> None:
    assert errors(pods(("m-0", "infra1"), ("m-1", "infra1")), ["infra1"]) == []


def test_a_pod_elsewhere_refuses_the_run() -> None:
    (err,) = errors(pods(("m-0", "infra1"), ("m-1", "app1")), ["infra1"])
    assert "m-1 runs on app1, outside node selector 'tier=infra'" in err


def test_no_pod_refuses_the_run() -> None:
    (err,) = errors(pods(), ["infra1"])
    assert "no pod matches" in err


def test_an_unread_api_refuses_the_run() -> None:
    (err,) = errors("", ["infra1"], rc=1)
    assert "cannot be read" in err
