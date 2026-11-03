"""The CDRs of a load incident's firing stopped with `abandon` (ADR-0049 §3.8).

Its generator was killed with its calls in flight and wrote no record for
them, so their CDRs join no record. They are joined against the firing
instead: its Call-IDs carry `<run>-<event>`, and its heal facts state the
calls it offered, those it did not place and those it failed itself, counted
at the hold before the kill (`held`). Each placed call owes exactly one CDR,
and a call the generator failed may or may not have reached the system under
test: a second distinct CDR of a Call-ID is `duplicate`, fewer Call-IDs than
the calls placed and not failed are as many `missing`, more than the calls
placed are as many `extra`, each charged at the instant the generator exited,
inside the firing's window. A firing whose facts hold no count taken at the
hold has only its duplicates charged, beside a `class_failed` finding
(cdr_check.py).
"""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from . import jsonval as jv
from .cdrjoin import DUPLICATE, EXTRA, MISSING, distinct, impact
from .classify import Impact
from .jsonl import read_jsonl
from .jsonval import Json


@dataclass(frozen=True)
class Firing:
    mark: str
    stream: str
    calls: int | None
    exited_at_ms: float
    not_placed: int = 0
    generator_failures: int = 0


def abandoned_firings(run_dir: Path, run: str) -> list[Firing]:
    out: list[Firing] = []
    for line in read_jsonl(run_dir / "events.jsonl"):
        facts = jv.obj(line, "facts")
        if line.get("kind") != "facts" or line.get("executable") != "heal" or facts.get("stop") != "abandon":
            continue
        out.append(
            Firing(
                mark=f"{run}-{jv.string(line, 'event')}",
                stream=jv.string(facts, "stream", ""),
                calls=jv.opt_int(facts, "calls") if facts.get("held") is True else None,
                exited_at_ms=jv.number(facts, "exited_at_ms", 0),
                not_placed=jv.opt_int(facts, "not_placed") or 0,
                generator_failures=jv.opt_int(facts, "generator_failures") or 0,
            )
        )
    return out


def split(cdrs: list[Json], firings: list[Firing]) -> tuple[list[Json], dict[str, list[Json]]]:
    """The CDRs of no abandoned firing, and those of each firing by its mark."""
    rest: list[Json] = []
    mine: dict[str, list[Json]] = defaultdict(list)
    for c in cdrs:
        call_id = str(c.get("call_id"))
        f = next((f for f in firings if f.mark in call_id), None)
        if f is None:
            rest.append(c)
        else:
            mine[f.mark].append(c)
    return rest, mine


def firing_impacts(firing: Firing, cdrs: list[Json], charge_missing: bool) -> list[Impact]:
    by_call: dict[str, list[Json]] = defaultdict(list)
    for c in distinct(cdrs):
        by_call[jv.string(c, "call_id")].append(c)
    at, s = firing.exited_at_ms, firing.stream
    out: list[Impact] = []
    for call_id, mine in sorted(by_call.items()):
        out += [impact(s, call_id, at, DUPLICATE, f"CDR {i + 2} of an abandoned call") for i in range(len(mine) - 1)]
    if firing.calls is None:
        return out
    placed = firing.calls - firing.not_placed
    missing = placed - firing.generator_failures - len(by_call)
    if missing > 0 and charge_missing:
        out += [impact(s, "", at, MISSING, "an abandoned call without a CDR") for _ in range(missing)]
    out += [impact(s, "", at, EXTRA, "a CDR beyond the calls the firing placed") for _ in range(len(by_call) - placed)]
    return out
