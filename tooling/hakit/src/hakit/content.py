"""Discovery of the search path's content, validated against its schemas.

Every directory of the search path may hold `incidents/<name>/incident.yaml`,
`budgets/<name>.yaml`, `profiles/<name>.yaml`, `catalogs/<name>.json`,
`classes/<name>.yaml`, `metrics/<name>.json`, `scenarios/` and
`dashboards/<file>.json`. A name (a dashboard's uid and a metric family
included) defined twice anywhere on the path is refused, naming both files
(ADR-0049 §1.3). A file that fails its schema is refused with its path and
the schema error (§1.4); a dashboard without the generic block and a malformed
metric catalogue likewise (§7.5, §7.7).
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from pathlib import Path

import yaml

from .classes import ProductClass
from .dashboards import read_dashboard
from .jsonval import Json, as_obj
from .metrics import read_catalogues
from .model import Budget, Catalog, Incident, Shape, parse_budget, parse_catalog, parse_incident
from .profile import Profile, parse_profile
from .schemas import SchemaSet


class ContentError(Exception):
    def __init__(self, errors: list[str]) -> None:
        super().__init__("\n".join(errors))
        self.errors = errors


@dataclass
class Content:
    path: list[Path]
    incidents: dict[str, Incident] = field(default_factory=lambda: {})
    budgets: dict[str, Budget] = field(default_factory=lambda: {})
    profiles: dict[str, Profile] = field(default_factory=lambda: {})
    catalogs: dict[str, Catalog] = field(default_factory=lambda: {})
    shapes: dict[str, Shape] = field(default_factory=lambda: {})
    classes: dict[str, ProductClass] = field(default_factory=lambda: {})
    scenarios: dict[str, Path] = field(default_factory=lambda: {})
    dashboards: dict[str, Path] = field(default_factory=lambda: {})
    dashboard_dirs: list[tuple[int, Path]] = field(default_factory=lambda: [])
    metric_families: dict[str, Path] = field(default_factory=lambda: {})


def _load(p: Path) -> object:
    text = p.read_text()
    return json.loads(text) if p.suffix == ".json" else yaml.safe_load(text)


class _Collector:
    def __init__(self, schemas: SchemaSet) -> None:
        self.schemas = schemas
        self.errors: list[str] = []
        self.origin: dict[tuple[str, str], Path] = {}

    def claim(self, kind: str, name: str, where: Path) -> bool:
        first = self.origin.get((kind, name))
        if first is not None:
            self.errors.append(f"{kind} {name!r} is defined twice: {first} and {where}")
            return False
        self.origin[(kind, name)] = where
        return True

    def document(self, family: str, p: Path, stem: str) -> Json | None:
        try:
            raw = _load(p)
        except (OSError, ValueError, yaml.YAMLError) as e:
            self.errors.append(f"{p}: cannot read: {e}")
            return None
        errs = self.schemas.errors_for_document(family, raw)
        if errs:
            self.errors.extend(f"{p}: {e}" for e in errs)
            return None
        d = as_obj(raw)
        if d.get("name") != stem:
            self.errors.append(f"{p}: name {d.get('name')!r} differs from the file name {stem!r}")
            return None
        return d


def _incidents(c: _Collector, root: Path, out: Content) -> None:
    for d in sorted(p for p in (root / "incidents").glob("*") if p.is_dir()):
        spec = d / "incident.yaml"
        if not spec.is_file():
            c.errors.append(f"{d}: no incident.yaml")
            continue
        doc = c.document("incident", spec, d.name)
        if doc is None or not c.claim("incident", d.name, spec):
            continue
        inc = parse_incident(doc, d)
        steps = ("inject", "heal") if inc.kind == "fault" else ()
        for step in steps:
            if inc.executable(step) is None:
                c.errors.append(f"{d}: a fault incident needs an executable {step!r}")
        for step in ("inject", "heal", "probe"):
            exe = inc.executable(step)
            if exe is not None and not os.access(exe, os.X_OK):
                c.errors.append(f"{exe}: not executable")
            if exe is not None and inc.kind == "load" and step != "probe":
                c.errors.append(f"{exe}: a load incident has no {step}; the runner drives its stream")
        out.incidents[inc.name] = inc


def _files(c: _Collector, root: Path, sub: str, family: str, suffix: str) -> list[tuple[Path, Json]]:
    found: list[tuple[Path, Json]] = []
    for p in sorted((root / sub).glob(f"*{suffix}")):
        doc = c.document(family, p, p.name.removesuffix(suffix))
        if doc is not None and c.claim(family, p.name.removesuffix(suffix), p):
            found.append((p, doc))
    return found


def _dashboards_and_metrics(c: _Collector, position: int, root: Path, out: Content) -> None:
    # Grafana's provider reads every directory below, into one folder: uids
    # and titles are both unique across the path.
    if (root / "dashboards").is_dir():
        out.dashboard_dirs.append((position, root / "dashboards"))
    for p in sorted((root / "dashboards").rglob("*.json")):
        uid, title, errs = read_dashboard(p)
        c.errors.extend(errs)
        if uid is None or errs or not c.claim("dashboard", uid, p):
            continue
        if c.claim("dashboard title", title, p):
            out.dashboards[uid] = p
    for p in sorted((root / "metrics").glob("*.json")):
        names, errs = read_catalogues(p)
        c.errors.extend(errs)
        for name in names:
            if c.claim("metric family", name, p):
                out.metric_families[name] = p


def discover(path: list[Path], schemas: SchemaSet) -> Content:
    """Read and validate every entry of the search path; raise `ContentError` on any refusal."""
    c = _Collector(schemas)
    out = Content(path=path)
    for root in path:
        if not root.is_dir():
            c.errors.append(f"{root}: search-path entry is not a directory")
            continue
        _incidents(c, root, out)
        for _, doc in _files(c, root, "budgets", "budget", ".yaml"):
            b = parse_budget(doc)
            out.budgets[b.name] = b
        for _, doc in _files(c, root, "profiles", "profile", ".yaml"):
            pr = parse_profile(doc)
            out.profiles[pr.name] = pr
        for p, doc in _files(c, root, "catalogs", "treatment-catalog", ".json"):
            cat = parse_catalog(doc)
            out.catalogs[cat.name] = cat
            for shape in cat.shapes:
                if c.claim("shape", shape.shape, p):
                    out.shapes[shape.shape] = shape
        for p, doc in _files(c, root, "classes", "impact-class", ".yaml"):
            name = str(doc["name"])
            count = p.with_suffix(".count")
            if not count.is_file() or not os.access(count, os.X_OK):
                c.errors.append(f"{p}: class {name!r} needs an executable {count.name}")
                continue
            out.classes[name] = ProductClass(name, str(doc["unit"]), count)
        for p in sorted((root / "scenarios").glob("*")):
            if p.is_file() and c.claim("scenario", p.name, p):
                out.scenarios[p.name] = p
        _dashboards_and_metrics(c, path.index(root), root, out)
    if c.errors:
        raise ContentError(c.errors)
    return out
