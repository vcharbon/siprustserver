"""JSON-lines files of a run directory: one object per line."""

from __future__ import annotations

import json
from collections.abc import Iterable
from pathlib import Path

from .jsonval import Json, as_obj


def read_jsonl(p: Path) -> list[Json]:
    """Every object of the file; none when it is absent."""
    if not p.is_file():
        return []
    return [as_obj(json.loads(line), p.name) for line in p.read_text().splitlines() if line.strip()]


def write_jsonl(p: Path, items: Iterable[Json]) -> None:
    with p.open("w") as fh:
        fh.writelines(json.dumps(i) + "\n" for i in items)


class JsonlLog:
    """A list whose every item is appended to its file as it is added, so a
    reader of a running run sees it at once."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.items: list[Json] = []

    def add(self, found: Iterable[Json]) -> None:
        new = list(found)
        if not new:
            return
        self.items += new
        with self.path.open("a") as fh:
            fh.writelines(json.dumps(i) + "\n" for i in new)
