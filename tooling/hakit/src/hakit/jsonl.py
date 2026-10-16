"""JSON-lines files of a run directory: one object per line."""

from __future__ import annotations

import json
from pathlib import Path

from .jsonval import Json, as_obj


def read_jsonl(p: Path) -> list[Json]:
    """Every object of the file; none when it is absent."""
    if not p.is_file():
        return []
    return [as_obj(json.loads(line), p.name) for line in p.read_text().splitlines() if line.strip()]
