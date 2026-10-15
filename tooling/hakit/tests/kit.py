"""Builders shared by the tests: search-path entries, records, run directories."""

from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from pathlib import Path

import yaml

from hakit.jsonval import Json

T0 = 1_800_000_000_000


def write_yaml(p: Path, doc: Json) -> Path:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(yaml.safe_dump(doc, sort_keys=False))
    return p


def executable(p: Path, body: str = "exit 0") -> Path:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(f"#!/bin/sh\n{body}\n")
    os.chmod(p, 0o755)
    return p


def fault_incident(root: Path, name: str, **extra: object) -> Path:
    d = root / "incidents" / name
    doc: Json = {
        "schema": "incident.v1",
        "name": name,
        "description": "d",
        "kind": "fault",
        "target": {"selector": "app=x"},
        **extra,
    }
    write_yaml(d / "incident.yaml", doc)
    executable(d / "inject")
    executable(d / "heal")
    return d


def budget(root: Path, name: str, **extra: object) -> Path:
    return write_yaml(root / "budgets" / f"{name}.yaml", {"schema": "budget.v1", "name": name, "description": "d", **extra})


def record(
    call_id: str,
    started: float,
    ending: str = "answered",
    *,
    stream: str = "basic",
    shape: str = "basic",
    run: str = "r",
    provisional: float | None = 20,
    setup: float | None = 100,
    answered: float | None = 100,
    ended: float | None = None,
    code: int | None = 200,
    reason: str | None = None,
    failures: Sequence[Mapping[str, object]] | None = None,
) -> Json:
    """A record whose instants are offsets in ms from `started`."""
    r: Json = {
        "schema": "call-outcome.v1",
        "run": run,
        "source": "sipp",
        "stream": stream,
        "shape": shape,
        "call_id": call_id,
        "started_at_ms": started,
        "first_provisional_at_ms": None if provisional is None else started + provisional,
        "setup_ended_at_ms": None if setup is None else started + setup,
        "answered_at_ms": None if answered is None else started + answered,
        "ended_at_ms": started + (ended if ended is not None else 30_000),
        "phase_reached": "released" if ending == "answered" else ("answered" if answered is not None else "provisional"),
        "early_media": False,
        "final_code": code,
        "reason": reason,
        "ending": ending,
        "failures": [dict(f) for f in failures or []],
    }
    return r


def rejected(call_id: str, started: float, code: int, reason: str, stream: str = "basic") -> Json:
    return record(call_id, started, "rejected", stream=stream, answered=None, code=code, reason=reason, ended=200)


def timed_out(call_id: str, started: float, deadline_ms: float = 32_000) -> Json:
    return record(call_id, started, "timeout", answered=None, setup=deadline_ms, code=None, reason="timeout", ended=deadline_ms)


def write_jsonl(p: Path, rows: list[Json]) -> None:
    p.write_text("".join(json.dumps(r) + "\n" for r in rows))
