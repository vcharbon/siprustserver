"""Static contract of the base incidents' executables (ADR-0049 §2.1, §2.3).

Every fault incident carries executable `inject` and `heal` scripts that
source the base helpers; every parameter an executable reads is declared in
its `incident.yaml` and every declared parameter is read; the shell passes
shellcheck. Needs `shellcheck` on PATH: without it the last test fails.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from pathlib import Path

import pytest
from exe_kit import BASE

from hakit.paths import FAULT_LIB

FAULTS = sorted(n for n, i in BASE.incidents.items() if i.kind == "fault")
LOADS = sorted(n for n, i in BASE.incidents.items() if i.kind == "load")
SOURCED = re.compile(r'^source "\$HAKIT_LIB/([a-z-]+\.sh)"$', re.M)
PARAM = re.compile(r"HAKIT_PARAM_([A-Z0-9_]+)")


def executables(name: str) -> list[Path]:
    d = BASE.incidents[name].directory
    return [d / s for s in ("inject", "heal", "probe") if (d / s).is_file()]


def read_params(name: str) -> set[str]:
    """Every HAKIT_PARAM_ an incident's executables read, with the helpers they source."""
    found: set[str] = set()
    for exe in executables(name):
        text = exe.read_text()
        found |= set(PARAM.findall(text))
        for lib in SOURCED.findall(text):
            found |= set(PARAM.findall((FAULT_LIB / lib).read_text()))
    return {p.lower() for p in found}


def test_the_base_library_covers_every_primitive_of_the_contract() -> None:
    assert set(FAULTS) >= {
        "kill_worker", "kill_worker_graceful", "kill_worker_held", "kill_proxy", "pod_kill", "force_delete", "pod_restart",
        "pod_freeze", "net_delay", "net_loss", "tcp_reset", "cpu_starve", "node_stop", "partition",
        "cut_external_plane", "sip_loss", "io_stall", "clock_skew",
    }
    assert {n: BASE.incidents[n].stop for n in LOADS} == {"spike": "drain", "abuse": "drain", "orphan": "abandon"}


def body(text: str) -> list[str]:
    return [ln for ln in text.splitlines() if ln.strip() and not ln.lstrip().startswith("#")]


@pytest.mark.parametrize("name", FAULTS)
def test_a_fault_sources_the_base_helpers_first(name: str) -> None:
    """A heal with nothing to undo is the bare `exit 0`; every other step
    sources incident.sh before anything else."""
    for exe in executables(name):
        text = exe.read_text()
        assert text.startswith("#!/usr/bin/env bash\n"), exe
        assert os.access(exe, os.X_OK), exe
        if body(text) == ["exit 0"]:
            assert exe.name == "heal", exe
            continue
        assert body(text)[0] == 'source "$HAKIT_LIB/incident.sh"', exe


@pytest.mark.parametrize("name", FAULTS)
def test_every_parameter_read_is_declared_and_every_declared_one_is_read(name: str) -> None:
    assert read_params(name) == set(BASE.incidents[name].params), name


@pytest.mark.parametrize("name", FAULTS)
def test_a_chaos_mesh_incident_requires_chaos_mesh(name: str) -> None:
    uses = any("chaos-mesh.sh" in SOURCED.findall(e.read_text()) for e in executables(name))
    assert uses == ("chaos-mesh" in BASE.incidents[name].requires), name


@pytest.mark.parametrize("name", FAULTS)
def test_a_node_or_cgroup_fault_requires_docker(name: str) -> None:
    text = " ".join(e.read_text() for e in executables(name))
    docker = re.search(r"\b(docker|pf_[a-z_]+|hk_timed_[a-z]+|hk_crash_[a-z]+)\b", text) is not None
    assert docker == ("docker" in BASE.incidents[name].requires), name


def test_the_shell_passes_shellcheck() -> None:
    sc = shutil.which("shellcheck")
    assert sc is not None, "shellcheck is not on PATH"
    files = [str(e) for n in FAULTS for e in executables(n)]
    files += [str(p) for p in sorted(FAULT_LIB.glob("*.sh")) if not p.is_symlink()]
    res = subprocess.run([sc, "-x", f"--source-path={FAULT_LIB}", *files], capture_output=True, text=True)
    assert res.returncode == 0, res.stdout
