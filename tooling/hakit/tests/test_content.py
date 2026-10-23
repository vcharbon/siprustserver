"""Search-path discovery: the base content is valid, and every refusal names its file."""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from kit import budget, fault_incident, write_yaml

from hakit.check import all_profile_errors
from hakit.content import ContentError, discover
from hakit.paths import BASE_CONTENT, search_path
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def refusal(path: list[Path]) -> list[str]:
    with pytest.raises(ContentError) as e:
        discover(path, SCHEMAS)
    return e.value.errors


def test_the_base_content_is_valid() -> None:
    content = discover([BASE_CONTENT], SCHEMAS)
    assert "kill_worker" in content.incidents
    assert "kill-worker-smoke" in content.profiles
    assert content.shapes["basic"].catalog == "x-api-call"
    assert all_profile_errors(content) == []


def test_the_search_path_is_the_base_then_hakit_path(tmp_path: Path) -> None:
    a, b = tmp_path / "a", tmp_path / "b"
    assert search_path({"HAKIT_PATH": f"{a}::{b}"}) == [BASE_CONTENT, a, b]
    assert search_path({}) == [BASE_CONTENT]


def test_a_name_defined_twice_is_refused_naming_both_files(tmp_path: Path) -> None:
    first = budget(tmp_path / "a", "worker-kill")
    errors = refusal([BASE_CONTENT, tmp_path / "a"])
    assert errors == [f"budget 'worker-kill' is defined twice: {BASE_CONTENT / 'budgets' / 'worker-kill.yaml'} and {first}"]


def test_an_incident_shadowing_a_base_incident_is_refused(tmp_path: Path) -> None:
    fault_incident(tmp_path, "kill_worker")
    errors = refusal([BASE_CONTENT, tmp_path])
    assert len(errors) == 1 and errors[0].startswith("incident 'kill_worker' is defined twice")


def test_a_file_failing_its_schema_is_refused_with_its_path(tmp_path: Path) -> None:
    p = budget(tmp_path, "b", call_cut={"max_calls": 1})
    errors = refusal([tmp_path])
    assert len(errors) == 1 and errors[0].startswith(f"{p}: (root): Additional properties are not allowed")


def test_a_name_differing_from_its_file_is_refused(tmp_path: Path) -> None:
    p = write_yaml(tmp_path / "budgets" / "b.yaml", {"schema": "budget.v1", "name": "c", "description": "d"})
    assert refusal([tmp_path]) == [f"{p}: name 'c' differs from the file name 'b'"]


def test_a_fault_incident_needs_executable_inject_and_heal(tmp_path: Path) -> None:
    d = fault_incident(tmp_path, "freeze")
    (d / "heal").unlink()
    os.chmod(d / "inject", 0o644)
    errors = refusal([tmp_path])
    assert f"{d}: a fault incident needs an executable 'heal'" in errors
    assert f"{d / 'inject'}: not executable" in errors


def test_a_load_incident_carries_no_inject(tmp_path: Path) -> None:
    d = tmp_path / "incidents" / "spike"
    write_yaml(d / "incident.yaml", {"schema": "incident.v1", "name": "spike", "description": "d", "kind": "load"})
    (d / "inject").write_text("#!/bin/sh\n")
    os.chmod(d / "inject", 0o755)
    assert refusal([tmp_path]) == [f"{d / 'inject'}: a load incident has no inject; the runner drives its stream"]
