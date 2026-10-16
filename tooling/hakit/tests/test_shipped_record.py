"""The one VictoriaLogs line shape of a call-outcome record (ADR-0049 §4.4),
shared with loadgen's sink tests through `fixtures/shipped-record.json`: the
SIPp record twin ships every record field at top level and the record whole
in `_msg`, and the runner reads the record back from `_msg`."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import ModuleType

from hakit.paths import SIPP_EXPORTER_DIR
from hakit.vlogs import VictoriaLogs

FIXTURE = json.loads((Path(__file__).parent / "fixtures" / "shipped-record.json").read_text())


def record_ship() -> ModuleType:
    spec = importlib.util.spec_from_file_location("record_ship", SIPP_EXPORTER_DIR / "record_ship.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_the_sipp_twin_ships_the_shared_line_shape() -> None:
    record = FIXTURE["record"]
    line, ending = record_ship().to_line(json.dumps(record), "run-1", "main", "basic")
    shipped = json.loads(line)
    assert sorted(shipped) == FIXTURE["fields"] and ending == "answered"
    assert json.loads(shipped["_msg"]) == record


def test_the_runner_reads_the_record_back_from_the_line() -> None:
    record = FIXTURE["record"]
    line, _ = record_ship().to_line(json.dumps(record), "run-1", "main", "basic")
    bad: list[str] = []
    assert VictoriaLogs._messages(line + "\n", bad) == [record] and bad == []  # pyright: ignore[reportPrivateUsage]
