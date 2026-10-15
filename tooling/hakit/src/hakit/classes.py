"""Impact classes a product adds on the search path (ADR-0049 §3.10).

`classes/<name>.yaml` (`impact-class.v1`) beside its executable
`classes/<name>.count`. The runner runs each count once after the export, with
`HAKIT_RUN_DIR` set, and keeps its answer as `classes/<name>.json` in the run
directory; the verdict reads that copy. A count that fails or prints anything
but a JSON array of `{at_ms, stream?, call_id?, detail?}` is `class_failed`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from . import jsonval as jv
from .classify import Impact
from .executor import Executor
from .jsonval import Json, as_obj

COUNT_TIMEOUT_S = 300


@dataclass(frozen=True)
class ProductClass:
    name: str
    unit: str
    count: Path


def _impacts(name: str, raw: object) -> list[Impact]:
    """The impacts of a count's output; ValueError when it is not the contract's array."""
    out: list[Impact] = []
    for i, item in enumerate(jv.as_list(raw, "count output")):
        d = as_obj(item, f"impact {i}")
        at = jv.number(d, "at_ms")
        out.append(
            Impact(
                name,
                jv.opt_string(d, "stream") or "",
                jv.opt_string(d, "call_id") or "",
                at,
                at,
                detail=jv.opt_string(d, "detail") or "",
            )
        )
    return out


def run_counts(classes: dict[str, ProductClass], run_dir: Path, executor: Executor) -> None:
    """Run every class's count over `run_dir` and keep each answer there."""
    out_dir = run_dir / "classes"
    out_dir.mkdir(exist_ok=True)
    for name, pc in sorted(classes.items()):
        res = executor.run(
            [str(pc.count)], env={"HAKIT_RUN_DIR": str(run_dir)}, cwd=run_dir, timeout_s=COUNT_TIMEOUT_S
        )
        doc: Json = {"name": name, "unit": pc.unit}
        if res.rc != 0:
            doc["error"] = f"exit {res.rc}: {res.stderr.strip()[:300]}"
        else:
            try:
                raw: object = json.loads(res.stdout)
                _impacts(name, raw)
                doc["impacts"] = raw
            except ValueError as e:
                doc["error"] = f"output is not an array of impacts: {e}"
        (out_dir / f"{name}.json").write_text(json.dumps(doc, indent=2) + "\n")


def read_counts(run_dir: Path) -> tuple[list[Impact], list[Json]]:
    """The impacts of every kept count, and a `class_failed` finding per failed one."""
    impacts: list[Impact] = []
    failed: list[Json] = []
    for p in sorted((run_dir / "classes").glob("*.json")):
        doc = as_obj(json.loads(p.read_text()), p.name)
        name = jv.string(doc, "name")
        err = jv.opt_string(doc, "error")
        try:
            got = _impacts(name, doc.get("impacts")) if err is None else []
        except ValueError as e:
            err = str(e)
            got = []
        if err is not None:
            failed.append({"kind": "class_failed", "detail": f"class {name}: {err}"})
        impacts += got
    return impacts, failed
