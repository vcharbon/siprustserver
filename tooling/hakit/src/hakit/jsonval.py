"""Typed reads of JSON values that a schema already validated.

A schema-valid document still reaches Python as untyped objects; these helpers
give each read its static type and fail loudly when the value has another one.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import cast

type Json = dict[str, object]


class ShapeError(ValueError):
    """A value is not of the type its schema promises."""


def as_obj(v: object, what: str = "value") -> Json:
    if not isinstance(v, dict):
        raise ShapeError(f"{what}: expected an object, got {type(v).__name__}")
    return cast(Json, v)


def as_list(v: object, what: str = "value") -> list[object]:
    if not isinstance(v, list):
        raise ShapeError(f"{what}: expected an array, got {type(v).__name__}")
    return cast(list[object], v)


def obj(d: Mapping[str, object], key: str) -> Json:
    return as_obj(d.get(key, {}), key)


def opt_obj(d: Mapping[str, object], key: str) -> Json | None:
    v = d.get(key)
    return None if v is None else as_obj(v, key)


def lst(d: Mapping[str, object], key: str) -> list[object]:
    return as_list(d.get(key, []), key)


def objs(d: Mapping[str, object], key: str) -> list[Json]:
    return [as_obj(x, key) for x in lst(d, key)]


def string(d: Mapping[str, object], key: str, default: str | None = None) -> str:
    v = d.get(key, default)
    if not isinstance(v, str):
        raise ShapeError(f"{key}: expected a string, got {type(v).__name__}")
    return v


def opt_string(d: Mapping[str, object], key: str) -> str | None:
    v = d.get(key)
    if v is None:
        return None
    if not isinstance(v, str):
        raise ShapeError(f"{key}: expected a string, got {type(v).__name__}")
    return v


def strings(d: Mapping[str, object], key: str) -> list[str]:
    out: list[str] = []
    for x in lst(d, key):
        if not isinstance(x, str):
            raise ShapeError(f"{key}: expected strings")
        out.append(x)
    return out


def number(d: Mapping[str, object], key: str, default: float | None = None) -> float:
    v = d.get(key, default)
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        raise ShapeError(f"{key}: expected a number, got {type(v).__name__}")
    return float(v)


def opt_number(d: Mapping[str, object], key: str) -> float | None:
    v = d.get(key)
    if v is None:
        return None
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        raise ShapeError(f"{key}: expected a number, got {type(v).__name__}")
    return float(v)


def opt_int(d: Mapping[str, object], key: str) -> int | None:
    v = d.get(key)
    if v is None:
        return None
    if isinstance(v, bool) or not isinstance(v, int):
        raise ShapeError(f"{key}: expected an integer, got {type(v).__name__}")
    return v


def boolean(d: Mapping[str, object], key: str, default: bool = False) -> bool:
    v = d.get(key, default)
    if not isinstance(v, bool):
        raise ShapeError(f"{key}: expected a boolean, got {type(v).__name__}")
    return v


def scalars(d: Mapping[str, object], key: str) -> dict[str, str | int | float | bool]:
    out: dict[str, str | int | float | bool] = {}
    for k, v in obj(d, key).items():
        if not isinstance(v, (str, int, float, bool)):
            raise ShapeError(f"{key}.{k}: expected a scalar")
        out[k] = v
    return out


def seq_of_str(v: Sequence[object]) -> list[str]:
    return [x for x in v if isinstance(x, str)]
