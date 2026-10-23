"""JSON-lines files of a run directory: one object per line."""

from __future__ import annotations

import json
from collections.abc import Iterable
from pathlib import Path

from .jsonval import Json, as_obj


def read_jsonl(p: Path) -> list[Json]:
    """Every object of the file; none when it is absent. The file is read line by
    line and every key and string value is one object shared by all the lines
    that hold it: a long run's records repeat the same names millions of times."""
    if not p.is_file():
        return []
    memo: dict[str, str] = {}

    def shared(pairs: list[tuple[str, object]]) -> Json:
        return {memo.setdefault(k, k): memo.setdefault(v, v) if isinstance(v, str) else v for k, v in pairs}

    with p.open() as fh:
        return [as_obj(json.loads(line, object_pairs_hook=shared), p.name) for line in fh if line.strip()]


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
