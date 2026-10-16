"""The CDRs of a load incident's firing stopped with `abandon` (ADR-0049 §3.8).

Its generator was killed with its calls in flight and wrote no record for
them, so their CDRs join no record. They are joined against the firing
instead: its Call-IDs carry `<run>-<event>`, and its heal facts state the
calls it placed. Each call owes exactly one CDR: a second distinct one of a
Call-ID is `duplicate`, fewer Call-IDs than calls placed are as many
`missing`, more are as many `extra`, each charged at the instant the
generator exited, inside the firing's window.
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
                calls=jv.opt_int(facts, "calls"),
                exited_at_ms=jv.number(facts, "exited_at_ms", 0),
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
    gap = firing.calls - len(by_call)
    if gap > 0 and charge_missing:
        out += [impact(s, "", at, MISSING, "an abandoned call without a CDR") for _ in range(gap)]
    out += [impact(s, "", at, EXTRA, "a CDR beyond the calls the firing placed") for _ in range(-gap)]
    return out
