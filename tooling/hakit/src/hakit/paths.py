"""Where the base's own files live, and the search path (ADR-0049 §1)."""

from __future__ import annotations

from collections.abc import Mapping
from pathlib import Path

# src/hakit/paths.py -> tooling/hakit
HAKIT_ROOT = Path(__file__).resolve().parents[2]
SCHEMAS_DIR = HAKIT_ROOT / "schemas"
BASE_CONTENT = HAKIT_ROOT / "content"
# The shell the base incidents source (`HAKIT_LIB`): the fault primitives
# (`pod-faults.sh`) and the incident helpers (`incident.sh`).
FAULT_LIB = HAKIT_ROOT / "lib"
# tooling/hakit -> the repository root, which holds the SIPp exporter twin.
REPO_ROOT = HAKIT_ROOT.parents[1]
SIPP_EXPORTER_DIR = REPO_ROOT / "deploy" / "k8s" / "sipp" / "exporter"


def search_path(env: Mapping[str, str]) -> list[Path]:
    """The base content followed by the `HAKIT_PATH` entries, in order."""
    entries = [BASE_CONTENT]
    for raw in env.get("HAKIT_PATH", "").split(":"):
        if raw.strip():
            entries.append(Path(raw).expanduser().resolve())
    return entries


def results_root(env: Mapping[str, str]) -> Path:
    """`HAKIT_RESULTS`, else a per-user state directory."""
    raw = env.get("HAKIT_RESULTS")
    if raw:
        return Path(raw).expanduser()
    return Path.home() / ".local" / "state" / "hakit" / "results"
