"""The SIPp injection file of a stream, rendered from its catalog shape.

One row per callee the lab offers, read in sequence. Field 0 is the R-URI user
part, fields 1 to `HEADER_SLOTS` each one header line `Name: value` (an unused
slot is empty, and SIPp drops the empty line it leaves). The placeholders a
catalog may use are `${callee_host}` and `${callee_port}`; a catalog naming
another is refused (ADR-0049 §5.1).
"""

from __future__ import annotations

import re

from .model import Catalog, Shape

HEADER_SLOTS = 4
DEFAULT_RURI_USER = "service"
FILLED = ("callee_host", "callee_port")
_PLACEHOLDER = re.compile(r"\$\{([a-z_][a-z0-9_]*)\}")


class InjectionError(ValueError):
    pass


def _fill(value: str, host: str, port: int) -> str:
    def sub(m: re.Match[str]) -> str:
        name = m.group(1)
        if name == "callee_host":
            return host
        if name == "callee_port":
            return str(port)
        raise InjectionError(f"placeholder ${{{name}}} is not one the SIPp renderer fills ({', '.join(FILLED)})")

    return _PLACEHOLDER.sub(sub, value)


def render(shape: Shape, catalog: Catalog, callees: list[tuple[str, int]]) -> str:
    unknown = sorted(set(catalog.placeholders) - set(FILLED))
    if unknown:
        raise InjectionError(f"catalog {catalog.name!r} names placeholders the SIPp renderer does not fill: {unknown}")
    if len(shape.headers) > HEADER_SLOTS:
        raise InjectionError(f"shape {shape.shape!r} has {len(shape.headers)} headers; the scenario holds {HEADER_SLOTS}")
    if not callees:
        raise InjectionError("the lab names no callee")
    rows = ["SEQUENTIAL"]
    for host, port in callees:
        fields = [_fill(shape.ruri_user or DEFAULT_RURI_USER, host, port)]
        fields += [f"{name}: {_fill(value, host, port)}" for name, value in shape.headers]
        fields += [""] * (HEADER_SLOTS - len(shape.headers))
        for f in fields:
            if ";" in f or "\n" in f or "\r" in f:
                raise InjectionError(f"shape {shape.shape!r}: {f!r} holds a field separator or a line break")
        rows.append(";".join(fields))
    return "\n".join(rows) + "\n"
