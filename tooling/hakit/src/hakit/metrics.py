"""The metric catalogues of the search path (ADR-0049 §7.7).

`metrics/<name>.json` holds catalogues in the `metric-contract export` format:
`{"catalogues": [{"binary", "families": [{"name", "kind", "labels",
"semi_open", "help"}]}]}`, `labels` a list of blocks of `{"name", "values"}`.
The name checker (`metric-contract check --content <entry>`) reads them
beside the shipped binaries' export; here each file is checked for the fields
that checker reads, and its family names are returned for the search path's
uniqueness rule.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

from .jsonval import ShapeError, as_list, as_obj

KINDS = ("counter", "gauge", "histogram")
_NAME = re.compile(r"^[a-zA-Z_:][a-zA-Z0-9_:]*$")
_LABEL = re.compile(r"^[a-zA-Z_][a-zA-Z0-9_]*$")


def _labels(name: str, blocks: object) -> list[str]:
    errs: list[str] = []
    listed = as_list(blocks, f"{name}: labels")
    if not listed:
        return [f"{name}: labels lists the label blocks (one empty block for none)"]
    for block in listed:
        for raw in as_list(block, f"{name}: a label block"):
            dim = as_obj(raw, f"{name}: a label")
            label = dim.get("name")
            if not isinstance(label, str) or not _LABEL.match(label):
                errs.append(f"{name}: label {label!r} is no label name")
            elif not all(isinstance(v, str) for v in as_list(dim.get("values"), f"{name}: {label}: values")):
                errs.append(f"{name}: label {label}: values are strings")
    return errs


def _family(raw: object) -> tuple[str | None, list[str]]:
    fam = as_obj(raw, "a family")
    name = fam.get("name")
    if not isinstance(name, str) or not _NAME.match(name):
        return None, [f"family name {name!r} is no metric name"]
    errs: list[str] = []
    if fam.get("kind") not in KINDS:
        errs.append(f"{name}: kind {fam.get('kind')!r} is none of {', '.join(KINDS)}")
    if not isinstance(fam.get("semi_open"), bool):
        errs.append(f"{name}: semi_open is a boolean")
    if not isinstance(fam.get("help"), str) or not fam.get("help"):
        errs.append(f"{name}: no help text")
    return name, errs + _labels(name, fam.get("labels"))


def read_catalogues(p: Path) -> tuple[list[str], list[str]]:
    """The family names the catalogue file `p` declares, and its refusals naming the file."""
    try:
        raw: object = json.loads(p.read_text())
    except (OSError, ValueError) as e:
        return [], [f"{p}: cannot read: {e}"]
    names: list[str] = []
    errs: list[str] = []
    try:
        cats = as_list(as_obj(raw, "the file").get("catalogues"), "catalogues")
        if not cats:
            return [], [f"{p}: no catalogues"]
        for c in cats:
            cat = as_obj(c, "a catalogue")
            if not isinstance(cat.get("binary"), str) or not cat.get("binary"):
                errs.append(f"{p}: a catalogue names its binary")
            for f in as_list(cat.get("families"), "families"):
                name, fam_errs = _family(f)
                errs.extend(f"{p}: {e}" for e in fam_errs)
                if name is not None and not fam_errs:
                    names.append(name)
    except ShapeError as e:
        errs.append(f"{p}: {e}")
    return names, errs
