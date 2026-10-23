"""JSON-lines files of a run directory."""

from __future__ import annotations

import json
from pathlib import Path

from hakit.jsonl import read_jsonl


def test_every_object_reads_back_and_repeated_strings_are_one_object(tmp_path: Path) -> None:
    rows = [{"stream": "basic", "call_id": f"c{i}", "nested": {"stream": "basic"}, "n": i} for i in range(3)]
    p = tmp_path / "records.jsonl"
    p.write_text("".join(json.dumps(r) + "\n" for r in rows) + "\n")
    got = read_jsonl(p)
    assert got == rows
    assert got[0]["stream"] is got[2]["stream"] is got[1]["nested"]["stream"]  # type: ignore[index]
    assert next(iter(got[0])) is next(iter(got[1]))


def test_an_absent_file_holds_nothing(tmp_path: Path) -> None:
    assert read_jsonl(tmp_path / "absent.jsonl") == []
