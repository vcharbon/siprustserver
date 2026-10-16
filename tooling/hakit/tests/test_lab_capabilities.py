"""`chaos-mesh` is read from the lab, never declared: a profile with a Chaos
Mesh incident is refused before the run on a lab whose API server does not
serve Chaos Mesh's resources."""

from __future__ import annotations

from pathlib import Path

import pytest
from kit import budget, write_yaml

from hakit.content import discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.jsonval import Json
from hakit.lab import lab_capabilities, lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner, RunRefused
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def no_crd(c: Call) -> ProcResult | None:
    return ProcResult(1, "", "NotFound") if c.kind == "run" and "crd" in c.argv else None


def test_a_declared_chaos_mesh_is_ignored_and_the_crd_decides() -> None:
    lab = lab_from_env({"HAKIT_LAB_CAPABILITIES": "kind,docker,chaos-mesh"})
    assert lab.capabilities == ["kind", "docker"]
    assert lab_capabilities(lab, FakeExecutor([no_crd])) == ["kind", "docker"]
    served = FakeExecutor([lambda c: ProcResult(0, "customresourcedefinition/networkchaos.chaos-mesh.org\n", "")])
    assert lab_capabilities(lab, served) == ["kind", "docker", "chaos-mesh"]
    assert served.calls[0].argv == ["kubectl", "--context", "kind-sip-e2e", "get", "crd", "networkchaos.chaos-mesh.org", "-o", "name"]


def test_a_chaos_mesh_incident_is_refused_on_a_lab_without_chaos_mesh(tmp_path: Path) -> None:
    root = tmp_path / "entry"
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1", "name": "cm", "description": "d", "duration_s": 120, "drain_s": 130, "settle_s": 15,
        "setup_slow_ms": 1000, "catalogs": ["x-api-call"],
        "streams": [{"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5,
                     "setup_deadline_s": 10, "ring_s": 5, "hold_s": 20}],
        "schedule": {"at": [{"at_s": 30, "incident": "partition", "budget": "any"}]},
    }
    write_yaml(root / "profiles" / "cm.yaml", doc)
    content = discover([BASE_CONTENT, root], SCHEMAS)
    clock, fake = simulated([no_crd])
    r = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    with pytest.raises(RunRefused, match=r"incident 'partition' requires \['chaos-mesh'\], which the lab lacks"):
        r.run("cm")
