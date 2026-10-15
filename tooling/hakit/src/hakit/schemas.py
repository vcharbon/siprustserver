"""The versioned schemas the base ships, and validation against them.

A document names its schema in its `schema` field (`incident.v1`); a major the
base does not ship is refused (ADR-0049 §1.6).
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import jsonschema

from .jsonval import ShapeError, as_obj
from .paths import SCHEMAS_DIR


@dataclass(frozen=True)
class SchemaSet:
    by_name: dict[str, dict[str, object]]

    @staticmethod
    def load(directory: Path = SCHEMAS_DIR) -> SchemaSet:
        found: dict[str, dict[str, object]] = {}
        for p in sorted(directory.glob("*.schema.json")):
            found[p.name.removesuffix(".schema.json")] = as_obj(json.loads(p.read_text()), p.name)
        return SchemaSet(found)

    def check_metaschema(self) -> list[str]:
        """Every schema against the 2020-12 metaschema."""
        errors: list[str] = []
        for name, schema in self.by_name.items():
            try:
                jsonschema.Draft202012Validator.check_schema(schema)
            except jsonschema.SchemaError as e:
                errors.append(f"{name}: {e.message}")
        return errors

    def errors(self, name: str, instance: object) -> list[str]:
        """Every validation error of `instance` against schema `name`, as text."""
        schema = self.by_name.get(name)
        if schema is None:
            return [f"unknown schema {name!r} (shipped: {', '.join(sorted(self.by_name))})"]
        validator: Any = jsonschema.Draft202012Validator(schema)
        found: list[Any] = list(validator.iter_errors(instance))
        out: list[str] = []
        for err in found:
            where = "/".join(str(p) for p in err.absolute_path) or "(root)"
            out.append(f"{where}: {err.message}")
        return sorted(out)

    def errors_for_document(self, family: str, instance: object) -> list[str]:
        """Validate a document whose `schema` field names `<family>.v<major>`."""
        try:
            declared = as_obj(instance).get("schema")
        except ShapeError:
            return ["(root): expected an object"]
        if not isinstance(declared, str) or not declared.startswith(f"{family}."):
            return [f"schema: expected {family}.v<major>, got {declared!r}"]
        if declared not in self.by_name:
            return [f"schema: {declared} is not a major this runner ships"]
        return self.errors(declared, instance)
